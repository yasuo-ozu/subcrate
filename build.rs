//! Build-script helper that turns `#[subcrate]` modules into real compiled
//! crates.
//!
//! Call [`compile`] from `build.rs`:
//!
//! ```ignore
//! fn main() {
//!     subcrate::build::compile().unwrap();
//! }
//! ```
//!
//! For finer control, build a [`Config`] with [`Config::from_env`], adjust it,
//! then call [`Config::run`].
//!
//! # How it works
//!
//! 1. Discover the package's source files, from its crate roots (`src/lib.rs`,
//!    `src/main.rs`, `src/bin/*.rs`, `tests/*.rs`, `benches/*.rs`, `examples/*.rs`,
//!    and the targets `Cargo.toml` declares) through `mod foo;` and
//!    `#[path = "…"]`. [Which roots](#crate-roots) depends on the manifest.
//! 2. Collect every `#[subcrate]` inline module.
//! 3. Resolve the dependencies between subcrates, sort them with [`safegraph`],
//!    and reject any cycle.
//! 4. If a subcrate uses an *owner* dependency — a crate the package depends on
//!    (`#[subcrate(::serde)]`) or the package's own library
//!    (`#[subcrate(::my_package)]`) — build those first (see
//!    [below](#owner-dependencies)).
//! 5. Compile each module with `rustc` (from `$RUSTC`), dependencies first and
//!    independent ones in parallel, reading the source from stdin — no temporary
//!    `.rs` files and no `Cargo.toml`s. The source is pretty-printed, so a
//!    diagnostic's `<anon>:<line>:<col>` points at the right construct rather than
//!    one long line. A dependency reaches its dependent as `--extern
//!    <internal>=<artifact>` plus an `extern crate … as <alias>;` at the
//!    dependent's crate root.
//! 6. Put the artifacts on the library search path and record metadata the macro
//!    reads to confirm the helper ran.
//!
//! The search path is emitted as `cargo:rustc-link-search` (the single-colon form
//! every Cargo back to the 1.56 MSRV accepts; `cargo::` is 1.77+). Cargo forwards
//! a build-script link path to dependent packages too, so publicly exported
//! subcrates resolve downstream with no extra wiring.
//!
//! # Crate roots
//!
//! The roots are the targets Cargo builds, found as Cargo finds them. The library is
//! `src/lib.rs`, or the `path` of `[lib]`. Binaries (`src/main.rs`, `src/bin/`),
//! integration tests (`tests/`), benchmarks (`benches/`) and examples (`examples/`)
//! are found by their place, and so are the `[[bin]]`, `[[test]]`, `[[bench]]` and
//! `[[example]]` the manifest declares: by their `path`, or by their `name` in the
//! place Cargo looks for it when there is none.
//!
//! Cargo's own switches apply: `autobins = false`, `autotests = false`,
//! `autobenches = false` and `autoexamples = false` in `[package]` turn the discovery
//! by place off for that kind of target, leaving the declared ones. Use them when one
//! of those directories holds files that are not this package's — for instance a
//! workspace whose root package keeps its tests in a package of their own, in
//! `tests/`. Cargo does not build those files as targets of this package, and neither
//! does this helper read them (nor does it re-run when they change). Left on, as they
//! are by default, every `*.rs` there is a root, and a `#[subcrate]` in it must
//! resolve within the package. (Cargo's `autolib` is not read: `src/lib.rs` is
//! always a root.)
//!
//! # Toolchain-conditional subcrates
//!
//! A `#[subcrate]` module may also carry a `rustversion` gate (`#[rustversion::nightly]`,
//! `#[rustversion::since(1.70)]`, …), which the `rustversion` crate uses to remove the
//! module from the *owner* crate on a non-matching toolchain. The helper does not
//! evaluate the gate — it only notices one is present (`is_rustversion_gate`) — and
//! treats a *gated* subcrate that fails to compile as "not available on this
//! toolchain": it is skipped rather than failing the build, and recorded as skipped
//! in the manifest so the macro expands it away. The compiler is thus the oracle; the
//! helper never has to agree with `rustversion` on what the current toolchain is. A
//! gated subcrate that fails where its gate *keeps* it still surfaces the compiler's
//! error (captured to `<internal>.skipreason` and shown by the macro). See
//! `Config::compile_all`.
//!
//! # Owner dependencies
//!
//! A subcrate is compiled *before* Cargo builds the package's own dependencies. So
//! to let a subcrate use one, the build script has Cargo build the crates it needs
//! (`cargo build -p <dep> … [-p <this package>] --lib`) from the package's
//! manifest directory, into a **separate target directory** — the main one is
//! locked by the build in progress. Running from the manifest directory makes
//! Cargo resolve the same versions and features as the outer build, so a type
//! from, say, `serde` means the same thing in the subcrate and in the owner.
//!
//! The owner itself (`-p <this package>`) joins that selection whenever a subcrate
//! uses the owner crate, and whenever it uses a dependency given by **path** — one
//! that might be a member of the same workspace. Under resolver 2, `cargo build -p
//! <dep>` resolves features for the selected packages alone, so for a workspace
//! member it would leave the owner out and drop any feature only the owner's graph
//! turns on (its build-dependencies — this crate enables `proc-macro2/span-locations`
//! — or, under `cargo test`, its dev-dependencies). The member would then be built
//! with a different hash than the main build's copy, and a type would not match
//! across the subcrate boundary (E0277). Selecting the owner unifies the two. A
//! registry dependency is never a member and is already resolved in the owner's
//! context, so it is not selected; a path dependency that turns out not to be a
//! member only costs a second build of the owner's library, never correctness.
//!
//! That directory, `subcrate-owner-deps/<package>/<compiler>`, is shared by every
//! configuration of the package (each has its own `OUT_DIR`), so the crates are
//! built once. It would otherwise duplicate the owner's dependency chain already in
//! the main target; since Cargo names an artifact by a content-and-config hash, the
//! copies are byte-identical, so the build script hard-links each to the main copy
//! to reclaim the disk (best effort; one `target/` tree, so one filesystem).
//!
//! The `<package>` component can be shared across a workspace: set
//! `SUBCRATE_OWNER_DEPS_KEY` (or [`Config::owner_deps_key`]) to one key for every
//! package, and they build the owner's dependencies into one directory instead of
//! once per package. The dedup step then runs under a lock file, so the packages'
//! build scripts do not rewrite each other's links at once. Only share it when the
//! packages resolve those crates to the same versions and features.
//!
//! `::name` is the dependency's key in `Cargo.toml`, or the package's own name. A
//! rename (`myleaf = { package = "leaf", … }`) builds the `package` but is seen
//! under the key. When the graph holds several versions, the build script picks the
//! one `Cargo.lock` resolves to (`-p leaf:1.2.3`, the spec form 1.56 accepts;
//! `leaf@1.2.3` is 1.77+), and links each version's own
//! `deps/lib<name>-<hash>.rlib` rather than the bare `lib<name>.rlib` that two
//! versions would share.
//!
//! Each crate reaches the subcrate as `--extern <name>=<rlib>` *and* `=<rmeta>`
//! (Cargo pipelines compilation, so an rlib alone is only a metadata stub), with
//! `-L` for the directories holding them — the same copies the package itself links
//! against under `cargo build`/`test`. (`cargo check` and clippy build other
//! copies; see the limitations below.) A library crate is bound with `pub extern
//! crate`, so a subcrate can re-export it (`pub use <alias>;`); a proc-macro crate,
//! which may export only its macros, is bound privately.
//!
//! Selecting the owner re-runs this build script inside that nested build, with
//! `SUBCRATE_NESTED` set. There are two reasons the owner is selected, handled
//! differently. (The variable, like `SUBCRATE_HARVEST_ONLY` below, names the package
//! whose build script set it, and only that package's build script honours it. The
//! nested build also runs the build scripts of the owner's dependencies, and one of
//! those that uses `subcrate` itself — a library with inline proc-macros, say —
//! builds its own subcrates as usual.)
//!
//! - **Only to unify a path dependency's features.** The owner's library is not
//!   wanted — just the dependency, built with the owner in the resolution — so the
//!   re-run does nothing, the throwaway library fails harmlessly at the
//!   `#[subcrate]` macro, and the dependency is read back from this `cargo build`'s
//!   JSON (exact path, resolved features, conflicts detected), not reconstructed
//!   from the file system.
//! - **Because a subcrate uses the owner crate (`::owner`).** The owner's library
//!   must compile, so the re-run compiles its subcrates and links *their* owner
//!   dependencies against the copies the enclosing build already placed under its
//!   profile directory, rather than starting a further nested build — one without
//!   the owner would undo the feature unification, one with it would not terminate.
//!
//! A subcrate in the library can't use the owner crate (it would depend on itself);
//! ones in `tests/`, `src/main.rs`, `src/bin/`, `examples/` and `benches/` can.
//!
//! A dependency may itself be backed by subcrates, as the owner is: its build
//! script compiles them in the nested build (into its `OUT_DIR`, as it would
//! anywhere), and the dependency's metadata refers to them. So the subcrate that
//! links it must be able to find them, and the build script forwards what the nested
//! build learns: the library search paths the build scripts of the crates built asked
//! for (`cargo:rustc-link-search`, read from the `build-script-executed` messages),
//! as far as they are in the nested build's own directory, go to every subcrate built
//! on owner dependencies — the place Cargo gives those paths to its own
//! compilations, but not to these. (When cross-compiling, those of both nested builds,
//! the host's included: see below.)
//!
//! # Cross-compiling
//!
//! When `--target` is not the host, a library subcrate is built for the target, like
//! the package, but a proc-macro subcrate is built for the host, and runs there. The
//! crates a proc-macro subcrate uses (`#[subcrate(proc_macro, ::syn)]`) are therefore
//! built for the host as well, by a second nested Cargo build of their own: `cargo
//! build -p <dep>… --lib --target <host>` into `subcrate-owner-deps/<package>/<compiler>-host`,
//! while the library subcrates' crates keep coming from the build for the target. A
//! crate used by both kinds is built twice. So a `#![no_std]` library for a bare-metal
//! target can have a proc-macro subcrate on `::syn`, a crate that could never be built
//! for that target. The host build is a nested build like the other: its crates' search
//! paths are forwarded as above, and a subcrate built on either build is given both.
//!
//! What this needs of the declaration of the crate in `Cargo.toml`: Cargo builds `-p
//! <dep>` only for a *normal dependency of the package that its resolution switches on
//! for the platform being built* (here the host's), and — as the nested build is
//! `--offline` — one that is in the local cache, which a crate the outer build never
//! builds is not. Cargo panics on anything else (see below). For a crate the
//! bare-metal build must not compile, that is a platform-gated normal dependency that
//! the host satisfies, which also puts the crate in the lock file and the cache when
//! the package is built for the host:
//!
//! ```toml
//! [target.'cfg(not(target_os = "none"))'.dependencies]
//! syn = "2"
//! ```
//!
//! An optional dependency whose feature is off in the build (`--no-default-features`)
//! does not qualify, nor does one that is only a build-dependency, nor does one that
//! a `[target.'cfg(…)'.dependencies]` table excludes on the host: Cargo then fails
//! (the nested build reports it, with this hint) — unless the dependency is a path
//! dependency that is a member of the same workspace, which Cargo builds whatever its
//! features. (So a proc-macro subcrate that needs a crate only a no-std build of the
//! package leaves out takes it from a platform-gated normal dependency as above, not
//! from an optional one.)
//!
//! A proc-macro subcrate cannot use the owner crate (`::owner`, built for the target)
//! nor a library subcrate (the same) when cross-compiling, and says so.
//!
//! # Packaged crates
//!
//! The nested build runs in the package's own directory, where Cargo finds the
//! workspace and `Cargo.lock` that govern it. That is right for a member of the
//! workspace being built, and for a source tree. For a crate *built from a registry*
//! (or a git checkout) it would not be: its own `Cargo.lock` is the one it was
//! published with, naming versions that need not be in the local cache — the nested
//! build is `--offline`, so it would fail for want of a download — and Cargo would
//! rewrite that file in the registry's source directory, which is not the build's to
//! write.
//!
//! So a crate that was packaged (it holds `Cargo.toml.orig`, or lies under Cargo's
//! `registry/src` or `git/checkouts`, and its manifest stands alone: no workspace, no
//! inherited field, no relative path dependency) gets a private workspace in the
//! build's profile directory instead: a `Cargo.toml` whose only member is a symbolic
//! link to the package, and a `Cargo.lock` of its own. The nested build runs with
//! `--manifest-path` of that workspace (from the package's directory still, for
//! Cargo's configuration files) and starts from, in this order:
//!
//! 1. the lock file of the workspace being built, found above `OUT_DIR`, when it
//!    lists the package as a dependency — the versions the outer build resolved, so the
//!    subcrate links the crates the package itself is compiled against;
//! 2. the package's own lock file, which is the right one when the package is what is
//!    being built (`cargo package`'s verification, docs.rs, `cargo install --locked`);
//! 3. none, so that Cargo resolves afresh from the crates in the cache.
//!
//! The next is tried when one fails. The package's own `Cargo.lock` is never touched.
//! (Where the symbolic link cannot be made, or Cargo's target directory is not under
//! the workspace so its lock file cannot be found, the nested build falls back to the
//! next choice, in the end to the package's directory as before.)
//!
//! `cargo package` verifies several packages of a workspace together (`cargo package
//! -p a -p b`) with a local overlay registry that holds the others, which only Cargo's
//! own process knows: a package that depends on a sibling that is not published yet
//! finds it nowhere in the nested build. The siblings are extracted beside the package,
//! in `target/package/`, so the private workspace points `[patch.crates-io]` at those
//! directories (for the registry packages of the lock file whose extracted sources
//! are there, when `target/package/tmp-registry` shows this is such a verification).
//!
//! # Limitations of `::name`
//!
//! - The crate must be an ordinary library, not a proc-macro.
//! - `::name` names the crate itself, not a path into it (`::serde::de` is an
//!   error).
//! - A dev-dependency is built as `cargo build` would, so its features can differ
//!   from the test build's (under resolver 2, if only a dev-dependency edge enables
//!   them).
//! - A custom profile is built as `dev` or `release` (a build script is only told
//!   which), so Cargo and the build script agree on the build.
//! - When cross-compiling, a proc-macro subcrate's crates are built for the host, so
//!   they must build there (the nested build for the host has no `RUSTFLAGS` of the
//!   target); but a proc-macro subcrate can't use the owner crate or a library
//!   subcrate: those are built for the target, a proc-macro for the host.
//! - The crate must be a normal dependency that the package's resolution switches on
//!   for the platform of the nested build (the target's, or the host's for a
//!   proc-macro subcrate), and the nested build runs `--offline`, so it must be in the
//!   local cache. An optional dependency whose feature is off, a build-dependency
//!   only, or one a `[target.'cfg(…)'.dependencies]` table leaves out for that
//!   platform, is not (see [Cross-compiling](#cross-compiling)). A workspace member
//!   (a path dependency) is built whatever its features.
//! - The nested build runs from the package's own directory, so it expects the sources
//!   in place; a source tree needs the workspace and `Cargo.lock` it is in (a
//!   packaged crate has a workspace of its own, see [Packaged crates](#packaged-crates)).
//! - `cargo package`'s verification of several packages works through a patch to the
//!   siblings' extracted sources, for `crates.io` dependencies only (not another
//!   registry).
//! - Under `cargo check` and clippy the dependency copies differ from
//!   `cargo build`'s, so a value of such a type crosses the subcrate boundary under
//!   `cargo build`/`test` but not under `cargo check`.
//! - A workspace member linked by `::name` is matched to the copy the owner's own
//!   build produces (the owner joins the nested selection, above). That is the main
//!   build's copy when the owner's graph alone sets the member's features — building
//!   the owner, or the whole workspace when no *other* member adds to that member's
//!   graph. If a sibling member turns on a feature in the member's graph that the
//!   owner's does not, a whole-workspace `cargo build` unifies it into the member
//!   while the nested build (owner plus the member) does not, and the copies differ
//!   again. Building the owner package on its own avoids this.
//! - If two versions of a crate (or two same-name, same-version crates from
//!   different sources) can't be told apart, the lock file can't say which `::name`
//!   means, and the build errors.
//! - When Cargo builds one `::name` twice with incompatible features (resolver 2,
//!   the normal and build/proc-macro graphs disagree), linking it is an error. This
//!   is reported clearly everywhere except one corner: a subcrate in the owner's
//!   *library* that links such a crate while *another* subcrate uses the owner crate
//!   (`::owner`). The owner's library is then recompiled in the nested build, where
//!   that library's own dependencies are matched from disk rather than from Cargo's
//!   feature metadata, so the (unavoidable) failure can surface as the nested
//!   build's diagnostic instead of the clearer "built more than once with
//!   incompatible features" message. Without a `::owner` in the package, the
//!   dependency is read from Cargo's JSON and the clear message is kept.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use safegraph::algo::toposort::toposort;
use safegraph::graph::Graph;
use safegraph::BTreeGraph;
use subcrate_core::{
    internal_crate_name, is_subcrate_attr, manifest_line, parse_attr_args, skipped_manifest_line,
    Dependency, ResolvedDep, UiExpectations,
};
use syn::{Attribute, Item, Meta};
use template_quote::quote;

/// Name of the metadata file (inside `OUT_DIR`) listing the compiled subcrates.
/// Must match the reader in `subcrate-macros`.
const MANIFEST_FILE: &str = "subcrate-manifest.txt";

/// Discovers and compiles every `#[subcrate]` module in the current package,
/// using the environment Cargo provides.
///
/// This is the one call `build.rs` needs:
///
/// ```ignore
/// fn main() {
///     subcrate::build::compile().unwrap();
/// }
/// ```
pub fn compile() -> Result<(), Error> {
    Config::from_env()?.run()
}

/// Configuration for the build helper.
///
/// Obtain one with [`Config::from_env`], optionally override fields, then call
/// [`Config::run`].
#[derive(Debug, Clone)]
pub struct Config {
    manifest_dir: PathBuf,
    out_dir: PathBuf,
    rustc: PathBuf,
    edition: String,
    /// Target triple, if cross-compiling (host != target).
    target: Option<String>,
    /// The host triple (`HOST`), set exactly when `target` is: what the nested build
    /// of the crates a proc-macro subcrate uses is built for (see
    /// [`build_host_deps`](Self::build_host_deps)).
    host: Option<String>,
    /// Panic strategy of the target (`unwind`/`abort`), applied to library
    /// subcrates so the final link is consistent.
    panic: Option<String>,
    opt_level: String,
    /// Whether `debug_assertions` (and, by extension, overflow checks) are on
    /// for this profile.
    debug_assertions: bool,
    /// The flags Cargo would pass to `rustc` for this package (`RUSTFLAGS`,
    /// `[build] rustflags`, target-specific flags, …), decoded from
    /// `CARGO_ENCODED_RUSTFLAGS`.
    rustflags: Vec<String>,
    /// `feature="…"` cfgs for the owning package's enabled features, so
    /// `#[cfg(feature = "…")]` inside a subcrate reflects the owner's feature
    /// selection. (Built-in target cfgs are set by `rustc` from `--target`;
    /// other custom cfgs arrive through `rustflags`.)
    feature_cfgs: Vec<String>,
    extra_rustc_args: Vec<String>,
    /// The `cargo` running this build (`CARGO`), used to build the owner's
    /// dependencies for the subcrates that use them.
    cargo: PathBuf,
    /// The owning package's name (`CARGO_PKG_NAME`).
    package: String,
    /// The owning package's version (`CARGO_PKG_VERSION`).
    package_version: String,
    /// Where the nested builds run, worked out once (see [`NestedWorkspace`]).
    nested_workspace: std::cell::RefCell<Option<NestedWorkspace>>,
    /// The build profile (`PROFILE`): `release` builds the owner's dependencies
    /// with `--release`, anything else without.
    profile: String,
    /// Whether this build script runs inside the nested Cargo build that *this
    /// package's* own `subcrate` build script started (`SUBCRATE_NESTED` names this
    /// package; see [`scope_key`]). The same variable also reaches the build scripts
    /// of the package's dependencies built in that nested build, which are not
    /// nested in this sense and so see `false`.
    nested: bool,
    /// Whether that nested build selected the owner only to unify a path
    /// dependency's features (`SUBCRATE_HARVEST_ONLY` names this package): the
    /// owner's own library need not compile here, so this run does nothing and lets
    /// it fail harmlessly. See [`build_owner_deps`](Self::build_owner_deps).
    harvest_only: bool,
    /// An opt-in key that replaces the package name in the nested owner-dependency
    /// target directory, so the packages of a workspace can share one such build
    /// instead of each rebuilding the owner's dependencies (`SUBCRATE_OWNER_DEPS_KEY`,
    /// or [`Config::owner_deps_key`]). `None` keeps the per-package default.
    owner_deps_key: Option<String>,
    /// The owner's dependencies (and the owner crate), built for the subcrates
    /// that use them. Filled in by [`Config::run`].
    owner_deps: OwnerDeps,
    /// When cross-compiling: the owner's dependencies built *for the host*, for the
    /// proc-macro subcrates that use them (a proc-macro runs on the host, so it
    /// cannot link a crate built for the target). Empty otherwise: the proc-macro
    /// subcrates then take theirs from `owner_deps`, like every other subcrate.
    host_deps: OwnerDeps,
}

/// Environment variable set on the nested Cargo build of the owner's
/// dependencies, so the owner's own `subcrate` build script, re-entered inside it,
/// can tell. Its value is the owner's [`scope_key`]: the nested build also runs the
/// build scripts of the owner's dependencies, and one of them that uses `subcrate`
/// too must not take the variable for its own.
const NESTED_ENV: &str = "SUBCRATE_NESTED";

/// Environment variable set on the nested build when the owner was selected only to
/// unify a path dependency's features, so the re-entered build script knows its
/// owner library is throwaway and does nothing. Scoped to the owner by its
/// [`scope_key`], like [`NESTED_ENV`]. See [`Config::build_owner_deps`].
const HARVEST_ONLY_ENV: &str = "SUBCRATE_HARVEST_ONLY";

/// What names a package in [`NESTED_ENV`] and [`HARVEST_ONLY_ENV`]: its manifest
/// directory, canonicalized (so the symlinks and `..`s of the path Cargo happens to
/// use in the nested build do not matter). One package, one directory — two packages
/// with the same name, or one package seen from two workspaces, are not mixed up.
fn scope_key(manifest_dir: &Path) -> OsString {
    canonical_path(manifest_dir).into_os_string()
}

/// Whether the environment variable `name` ([`NESTED_ENV`] or [`HARVEST_ONLY_ENV`])
/// was set for the package `scope` names ([`scope_key`]), and only for that one.
/// Another package's value is not: the variable reaches the build scripts of every
/// package the nested build compiles.
fn env_is_for(name: &str, scope: &OsString) -> bool {
    std::env::var_os(name).map_or(false, |value| &value == scope)
}

impl Config {
    /// Builds a [`Config`] from the environment variables Cargo sets for build
    /// scripts, so subcrates compile with the same configuration as the package
    /// that owns them: `CARGO_MANIFEST_DIR`, `OUT_DIR`, `RUSTC`, `TARGET`,
    /// `HOST`, `OPT_LEVEL`, `PROFILE`, `CARGO`, `CARGO_PKG_NAME`,
    /// `CARGO_CFG_PANIC`, `CARGO_CFG_DEBUG_ASSERTIONS` and
    /// `CARGO_ENCODED_RUSTFLAGS`, plus the package's edition from `Cargo.toml`.
    pub fn from_env() -> Result<Self, Error> {
        let manifest_dir = PathBuf::from(require_env("CARGO_MANIFEST_DIR")?);
        let out_dir = PathBuf::from(require_env("OUT_DIR")?);
        let package = require_env("CARGO_PKG_NAME")?;
        let package_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
        let rustc = PathBuf::from(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()));
        let cargo = PathBuf::from(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
        let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
        // Both variables are for the package whose build script set them: the nested
        // build runs the build scripts of its dependencies too, and one of those that
        // uses `subcrate` builds its own subcrates as usual.
        let scope = scope_key(&manifest_dir);
        let nested = env_is_for(NESTED_ENV, &scope);
        let harvest_only = env_is_for(HARVEST_ONLY_ENV, &scope);

        let target = std::env::var("TARGET").ok();
        let host = std::env::var("HOST").ok();
        // Only pass `--target` when genuinely cross-compiling; otherwise the
        // host default already matches and matches how Cargo invokes rustc.
        let (cross_target, cross_host) = match (&target, &host) {
            (Some(t), Some(h)) if t != h => (Some(t.clone()), Some(h.clone())),
            _ => (None, None),
        };

        let edition = detect_edition(&manifest_dir);
        let panic = std::env::var("CARGO_CFG_PANIC").ok();
        let opt_level = std::env::var("OPT_LEVEL").unwrap_or_else(|_| "0".into());
        // `CARGO_CFG_DEBUG_ASSERTIONS` is present (with an empty value) exactly
        // when the profile enables debug assertions.
        let debug_assertions = std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_some();
        let rustflags = decode_rustflags();
        let feature_cfgs = enabled_feature_cfgs(&manifest_dir);
        let owner_deps_key = std::env::var("SUBCRATE_OWNER_DEPS_KEY")
            .ok()
            .filter(|s| !s.is_empty());

        Ok(Config {
            manifest_dir,
            out_dir,
            rustc,
            edition,
            target: cross_target,
            host: cross_host,
            panic,
            opt_level,
            debug_assertions,
            rustflags,
            feature_cfgs,
            extra_rustc_args: Vec::new(),
            cargo,
            package,
            package_version,
            nested_workspace: std::cell::RefCell::new(None),
            profile,
            nested,
            harvest_only,
            owner_deps_key,
            owner_deps: OwnerDeps::default(),
            host_deps: OwnerDeps::default(),
        })
    }

    /// Overrides the edition used to compile subcrates (defaults to the
    /// package's edition).
    pub fn edition(mut self, edition: impl Into<String>) -> Self {
        self.edition = edition.into();
        self
    }

    /// Adds an extra argument passed to every `rustc` invocation.
    pub fn extra_rustc_arg(mut self, arg: impl Into<String>) -> Self {
        self.extra_rustc_args.push(arg.into());
        self
    }

    /// Shares the nested owner-dependency build across packages that pass the same
    /// `key`, instead of giving each package its own copy (the default). Every
    /// package in a workspace that sets the same key builds the owner's
    /// dependencies into one directory, so they are compiled once rather than once
    /// per package. The key is one path component — non-alphanumeric characters
    /// (path separators included) become `_` — and the per-compiler, per-profile
    /// split is kept, so toolchains and profiles still do not mix.
    ///
    /// Also set from the `SUBCRATE_OWNER_DEPS_KEY` environment variable. Only use it
    /// when the packages genuinely resolve the shared crates to the same versions
    /// and features; otherwise leave it unset.
    pub fn owner_deps_key(mut self, key: impl Into<String>) -> Self {
        self.owner_deps_key = Some(key.into());
        self
    }

    /// Runs discovery and compilation, emitting the necessary Cargo directives.
    pub fn run(mut self) -> Result<(), Error> {
        // This run is the owner's library being compiled inside a nested build that
        // selected the owner only to unify a path dependency's features. That copy
        // of the library is thrown away — the enclosing build harvests the
        // dependency from Cargo's JSON, not from here — so do nothing. Emitting no
        // manifest makes the library's `#[subcrate]` macro fail, which is expected
        // and ignored. See [`build_owner_deps`](Self::build_owner_deps).
        if self.harvest_only {
            return Ok(());
        }

        let subcrates_dir = self.out_dir.join("subcrates");
        std::fs::create_dir_all(&subcrates_dir)?;

        let discovered = discover(&self.manifest_dir);

        // Rebuild whenever the manifest or any discovered source file changes,
        // or when the flags we forward to subcrates change.
        rerun_if_changed(&self.manifest_dir.join("Cargo.toml"));
        // Watch the root source directories too: a Cargo `rerun-if-changed` on a
        // directory fires when files are *added or removed*, so a brand-new file
        // containing a `#[subcrate]` re-runs discovery. (Per-file tracking alone
        // would miss files that did not exist on the previous run.) A directory the
        // manifest turns auto-discovery off for is not watched: what is in it is not
        // this package's, and its changes are none of this build script's business.
        let auto = std::fs::read_to_string(self.manifest_dir.join("Cargo.toml"))
            .map(|manifest| auto_discovery(&manifest))
            .unwrap_or_default();
        for dir in auto.root_dirs() {
            let path = self.manifest_dir.join(dir);
            if path.is_dir() {
                rerun_if_changed(&path);
            }
        }
        // Plus each discovered file, to catch `#[path]` files outside those roots.
        for file in &discovered.files {
            rerun_if_changed(file);
        }
        println!("cargo:rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");

        // Resolve who depends on whom, then compile in dependency order so every
        // dependency's artifact exists before its dependents are compiled.
        let subcrates = &discovered.subcrates;
        let owner = self.owner();
        let links = resolve_links(subcrates, &owner)?;
        let needs = compile_deps(subcrates, &links, &owner);
        let order = compile_order(subcrates, &needs)?;

        // A nested build only builds the library, so only the subcrates it uses
        // matter; everything else (notably anything using the owner crate, which
        // is what is being built) is left alone.
        let wanted = if self.nested {
            needed_by_lib(subcrates, &needs, &owner)
        } else {
            vec![true; subcrates.len()]
        };
        // Build whatever the wanted subcrates take from the owner's dependencies
        // (or the owner itself) before compiling any of them.
        self.owner_deps = self.build_owner_deps(subcrates, &links, &wanted)?;
        // When cross-compiling, the proc-macro subcrates take theirs from a build for
        // the host instead.
        self.host_deps = self.build_host_deps(subcrates, &links, &wanted)?;

        let owner_linked = owner_linked(&links, &order);
        // `gate_skipped[ix]`: the subcrate carries a `#[rustversion]` gate and did
        // not build on this toolchain, so it is recorded as skipped below rather
        // than linked (see [`Config::compile_all`]).
        let gate_skipped = self.compile_all(
            subcrates,
            &links,
            &needs,
            &wanted,
            &subcrates_dir,
            &owner_linked,
        )?;
        let internal_names: Vec<String> = subcrates.iter().map(|s| s.internal.clone()).collect();

        prune_stale(&subcrates_dir, &internal_names);

        // Make the subcrate artifacts resolvable while compiling this package.
        // A kindless search path becomes `-L PATH` (the `all` kind), which rustc
        // searches for directly-named `extern crate`s.
        println!("cargo:rustc-link-search={}", subcrates_dir.display());
        // A subcrate built on an owner dependency records the exact copy of that
        // crate it was built against. Whatever loads the subcrate (a test, a
        // binary) looks that copy up by name and hash on the search path, and
        // Cargo's own search path may not hold it: the package's own library is
        // only handed over with `--extern`, and `cargo check` builds different
        // copies than the `cargo build` ones used here. So point to the copies
        // built here, which otherwise are the same crates the package is built
        // with.
        for dir in self.dependency_dirs() {
            println!("cargo:rustc-link-search=dependency={}", dir.display());
        }

        // Metadata the procedural macro reads to confirm the helper ran.
        let manifest = self.out_dir.join(MANIFEST_FILE);
        let mut body = String::new();
        for (ix, sub) in subcrates.iter().enumerate() {
            if gate_skipped[ix] {
                // No artifact exists; the macro expands this subcrate away (and
                // reads `<internal>.skipreason` if it still reaches the module).
                body.push_str(&skipped_manifest_line(&sub.internal));
            } else {
                body.push_str(&manifest_line(
                    &sub.internal,
                    &doc_deps(subcrates, ix, &links[ix]),
                ));
            }
            body.push('\n');
        }
        std::fs::write(&manifest, body)?;
        println!(
            "cargo:rustc-env=SUBCRATE_MANIFEST_DIR={}",
            self.out_dir.display()
        );

        Ok(())
    }

    /// The package's library, as dependency resolution sees it.
    fn owner(&self) -> Owner {
        // `[lib] path = "…"` if the manifest has one, else the default `src/lib.rs`.
        // The same path as `crate_roots` builds, so a subcrate in the library is
        // still recognised as part of it.
        let lib = std::fs::read_to_string(self.manifest_dir.join("Cargo.toml"))
            .ok()
            .and_then(|manifest| lib_source_path(&manifest))
            .map(|rel| self.manifest_dir.join(rel))
            .unwrap_or_else(|| self.manifest_dir.join("src").join("lib.rs"));
        Owner {
            name: self.owner_crate_name(),
            // Canonical, to match a library subcrate's (canonical) crate root even
            // when the `[lib] path` and the discovered path are spelled differently.
            lib: if lib.is_file() {
                Some(canonical_path(&lib))
            } else {
                None
            },
        }
    }

    /// The name of the package's library crate: the package name, with `-` as `_`.
    fn owner_crate_name(&self) -> String {
        self.package.replace('-', "_")
    }

    /// The linked dependencies of `subcrates[ix]` (from its resolved `links`),
    /// with the artifact paths found in `dir`, in a stable order. `owner_linked`
    /// says which subcrates (by index) are built on owner dependencies.
    fn extern_deps(
        &self,
        subcrates: &[Subcrate],
        ix: usize,
        links: &[Link],
        dir: &Path,
        owner_linked: &[bool],
    ) -> Result<Vec<ExternDep>, Error> {
        let sub = &subcrates[ix];
        let mut deps = Vec::new();
        for link in links {
            match &link.target {
                LinkTarget::Subcrate(target) => {
                    let dep = &subcrates[*target];
                    // A proc-macro crate runs on the host, but a library subcrate is
                    // built for the target; when those differ the host cannot load it.
                    if sub.proc_macro && !dep.proc_macro && self.target.is_some() {
                        return Err(Error::new(format!(
                            "proc-macro subcrate `{}` cannot depend on library subcrate `{}` \
                             when cross-compiling: the library is built for the target, \
                             but a proc-macro runs on the host",
                            sub.display_name(),
                            dep.display_name()
                        )));
                    }
                    deps.push(ExternDep {
                        internal: dep.internal.clone(),
                        alias: link.alias.clone(),
                        paths: vec![dir.join(dep.artifact_file())],
                        owner_linked: owner_linked[*target],
                    });
                }
                LinkTarget::External(name) => {
                    deps.push(self.external_dep(sub, name, &link.alias)?);
                }
                LinkTarget::Owner => {
                    self.check_owner_use(sub)?;
                    deps.push(self.external_dep(sub, &self.owner_crate_name(), &link.alias)?);
                }
            }
        }
        Ok(deps)
    }

    /// Whether `sub` is built for the host while the rest of the package is built for
    /// the target: a proc-macro subcrate of a cross-compiled package. (It runs inside
    /// the compiler, so the crates it uses are the host's.)
    fn is_host_built(&self, sub: &Subcrate) -> bool {
        sub.proc_macro && self.target.is_some()
    }

    /// The error for a proc-macro subcrate that uses the owner crate (`::owner`) when
    /// cross-compiling: the owner's library is built for the target, which a
    /// proc-macro (running on the host) cannot load.
    fn check_owner_use(&self, sub: &Subcrate) -> Result<(), Error> {
        if self.is_host_built(sub) {
            return Err(Error::new(format!(
                "proc-macro subcrate `{}` cannot depend on the owner crate `::{}` when \
                 cross-compiling: it is built for the target, but a proc-macro runs on the host",
                sub.display_name(),
                self.owner_crate_name()
            )));
        }
        Ok(())
    }

    /// The owner dependencies `sub` links: the host's when it is
    /// [built for the host](Self::is_host_built), else the target's.
    fn deps_of(&self, sub: &Subcrate) -> &OwnerDeps {
        if self.is_host_built(sub) {
            &self.host_deps
        } else {
            &self.owner_deps
        }
    }

    /// Every directory holding an owner dependency's artifacts, of both builds: the
    /// search paths for those crates' own dependencies. A library subcrate may use a
    /// proc-macro subcrate built on a host crate (and the other way round), so a
    /// subcrate that is built on either needs to find the other's too.
    fn dependency_dirs(&self) -> impl Iterator<Item = &PathBuf> {
        self.owner_deps
            .lib_dirs
            .iter()
            .chain(self.host_deps.lib_dirs.iter())
    }

    /// Every library search path the build scripts of the crates of both builds asked
    /// for (see [`OwnerDeps::script_paths`]), but those that are [`dependency_dirs`]
    /// already. As for those, a subcrate built on either build may need the other's:
    /// a proc-macro subcrate that uses a host crate backed by subcrates finds them
    /// there, and so does a library subcrate that uses that proc-macro subcrate.
    ///
    /// [`dependency_dirs`]: Self::dependency_dirs
    fn script_dirs(&self) -> impl Iterator<Item = &PathBuf> {
        self.owner_deps
            .script_paths
            .union(&self.host_deps.script_paths)
            .filter(|dir| {
                !self.owner_deps.lib_dirs.contains(*dir) && !self.host_deps.lib_dirs.contains(*dir)
            })
    }

    /// The link to the crate `name` built by [`Config::build_owner_deps`] (or, for a
    /// proc-macro subcrate of a cross-compiled package, by
    /// [`Config::build_host_deps`]), seen as `alias` by `sub`.
    fn external_dep(&self, sub: &Subcrate, name: &str, alias: &str) -> Result<ExternDep, Error> {
        let owner_deps = self.deps_of(sub);
        // `name` is what the subcrate's author wrote, which is not necessarily the
        // library crate it stands for (a dependency may be renamed), nor the only
        // version of that crate Cargo built.
        let external = owner_deps.externals.get(name);
        let crate_name = external.map_or(name, |external| external.crate_name.as_str());
        let krate = external
            .and_then(|external| external.version.as_ref())
            .and_then(|version| {
                let key = (crate_name.to_string(), version.clone());
                owner_deps.versions.get(&key)
            })
            .or_else(|| owner_deps.crates.get(crate_name))
            .filter(|krate| krate.rlib.is_some())
            .ok_or_else(|| {
                Error::new(format!(
                    "subcrate `{}` depends on `::{}`, but building it produced no library \
                     artifact (only ordinary library crates can be used this way)",
                    sub.display_name(),
                    name
                ))
            })?;
        // Under resolver 2 a crate can be built more than once with different
        // features; refuse to guess which build the subcrate meant.
        if let Some(other) = &krate.conflict {
            return Err(Error::new(format!(
                "subcrate `{}` depends on `::{}`, but Cargo built it more than once with \
                 incompatible features — [{}] and [{}], neither a superset of the other — so \
                 which build to link is ambiguous. This happens under resolver 2 when a crate's \
                 features differ between the normal and build-dependency graphs; use \
                 `resolver = \"1\"`, or arrange for it to be built once.",
                sub.display_name(),
                name,
                krate.features.join(", "),
                other.join(", "),
            )));
        }
        // Cargo pipelines compilation: the rlib alone has only a metadata stub, so
        // the real `.rmeta` has to be named as well.
        let paths = krate
            .rlib
            .iter()
            .chain(krate.rmeta.iter())
            .cloned()
            .collect();
        Ok(ExternDep {
            internal: name.to_string(),
            alias: alias.to_string(),
            paths,
            owner_linked: true,
        })
    }

    /// Builds the crates the `wanted` subcrates take from the owner — crates the
    /// package depends on (`::name`) and the owner crate itself — and returns their
    /// artifacts. Builds nothing if no wanted subcrate uses any.
    ///
    /// All of them come from one nested Cargo build into a separate target
    /// directory (see the [module documentation](self)). When cross-compiling, that
    /// build is for the target and serves the library subcrates; the proc-macro
    /// subcrates' crates are built for the host by [`build_host_deps`](Self::build_host_deps).
    fn build_owner_deps(
        &self,
        subcrates: &[Subcrate],
        links: &[Vec<Link>],
        wanted: &[bool],
    ) -> Result<OwnerDeps, Error> {
        // The crates in use, each with the first subcrate to use it (for errors).
        let mut externals: BTreeMap<&str, String> = BTreeMap::new();
        let mut owner_user: Option<String> = None;
        for (ix, links) in links.iter().enumerate() {
            if !wanted[ix] || self.is_host_built(&subcrates[ix]) {
                continue;
            }
            for link in links {
                let user = || subcrates[ix].display_name();
                match &link.target {
                    LinkTarget::External(name) => {
                        externals.entry(name).or_insert_with(user);
                    }
                    LinkTarget::Owner => {
                        owner_user.get_or_insert_with(user);
                    }
                    LinkTarget::Subcrate(_) => {}
                }
            }
        }
        if externals.is_empty() && owner_user.is_none() {
            return Ok(OwnerDeps::default());
        }

        // Building the owner crate would re-enter this build script, which would
        // build the owner crate again, and so on. `needed_by_lib` keeps a nested
        // build from getting here for the owner; this is the backstop.
        if self.nested {
            if let Some(user) = &owner_user {
                return Err(Error::new(format!(
                    "subcrate `{}` depends on the owner crate `::{}` but is being built as part \
                     of it: that is a dependency cycle",
                    user,
                    self.owner_crate_name()
                )));
            }
        }

        let lock = self.lock_for_specs();
        let ResolvedExternals {
            mut packages,
            crates,
            links_local_dep,
        } = self.resolve_externals(&externals, &lock)?;
        // A nested run that reaches here is building the owner's library because a
        // subcrate uses the *owner crate* (`::owner`): the harvest-only nested build,
        // which selects the owner merely to unify a dependency's features, returned
        // from `run` before this. The enclosing build already compiled every crate
        // this library's subcrates need — with the owner's feature unification, so
        // the copies match the main build — into its own deps directory, this run's
        // `main_profile_dir()`. Link those rather than launch another nested build:
        // one selecting the dependency without the owner would bring back the
        // mismatched hash, and one selecting the owner again would recurse forever.
        if self.nested {
            return self.owner_deps_from_enclosing(&crates);
        }

        // Put the owner package itself in the nested selection when a dependency it
        // links is given by path, and so might be a member of the same workspace.
        // Under resolver 2, `cargo build -p <dep>` resolves features for the selected
        // packages alone. For a workspace member that leaves the owner out of the
        // resolution, dropping any feature only the owner's graph turns on — its
        // build-dependencies (`subcrate` enables `proc-macro2/span-locations`), or
        // under `cargo test` its dev-dependencies. The member is then built with a
        // different feature set, and so a different metadata hash, than the main
        // build's copy, and a type crossing the subcrate boundary does not match it
        // (E0277, "there are multiple different versions of crate `…`"). Selecting
        // the owner too unifies features across both, so the member gets the main
        // build's copy. A registry dependency is never a member and is already
        // resolved with the owner's graph, so it is left alone; keying on `path`
        // rather than true membership may still include a path dependency that is not
        // a member, which only costs the harvest below for nothing — never wrong.
        if (owner_user.is_some() || links_local_dep) && !packages.contains(&self.package) {
            packages.push(self.package.clone());
        }

        // When the owner is in the selection *only* for that feature unification —
        // no subcrate uses the owner crate itself — its own library need not compile
        // here. Tell the nested run so (it then does nothing and the library fails
        // harmlessly at the `#[subcrate]` macro); the dependency is read back from
        // this build's JSON below, with its copy and features decided by Cargo's
        // resolution rather than guessed from the file system. With `::owner` in play
        // the library must build, so this stays off and the re-entry links the
        // copies from disk (`owner_deps_from_enclosing`, above).
        let harvest_only = links_local_dep && owner_user.is_none();

        let target_dir = self.owner_deps_target_dir();
        let output = self.run_nested(&packages, &target_dir, harvest_only, false, &|output| {
            built_nested(output, harvest_only, &crates)
        })?;

        // Reclaim the disk the nested build just spent. It re-stored the owner's
        // dependency chain in its own target directory, byte for byte the same as
        // the copies already in the main target: Cargo names an artifact by a hash
        // of its content and configuration, so one name is one content. Replace
        // each duplicate with a hard link to the main copy — both live in the same
        // `target/` tree, hence on the same filesystem, and a shared hash name can
        // never come to mean two different contents, so the shared inode is safe.
        // Best effort: any I/O failure just leaves the extra copy in place.
        if let Some(main) = self.main_profile_dir() {
            // Serialize the dedup across build scripts sharing this directory (see
            // `owner_deps_key`, and the same package's parallel `build`/`test`),
            // so they never rewrite each other's links at once. Uncontended — the
            // per-package default — acquisition is immediate.
            let _lock = DedupLock::acquire(&target_dir);
            dedup_against_main(&target_dir, &main);
        }

        Ok(self.nested_deps(&output, &target_dir, crates))
    }

    /// Builds, when cross-compiling, the crates the `wanted` proc-macro subcrates take
    /// from the owner — crates the package depends on (`::name`) — *for the host*, and
    /// returns their artifacts. Builds nothing when not cross-compiling (a proc-macro
    /// subcrate then uses the crates of [`build_owner_deps`](Self::build_owner_deps)
    /// like any other) or when no wanted proc-macro subcrate uses any.
    ///
    /// A proc-macro runs inside the compiler, on the host, so it can only link crates
    /// built for the host; the target's copies (which serve the library subcrates, the
    /// part of the package that ships) could not even be loaded. So this is a second
    /// nested Cargo build, of just those crates, into a target directory of its own
    /// (`subcrate-owner-deps/<package>/<compiler>-host`) and with `--target` set to the
    /// host triple. (An explicit triple rather than none, so a `build.target` in a
    /// Cargo configuration or `CARGO_BUILD_TARGET` in the environment cannot send it to
    /// the target after all.) The target's `RUSTFLAGS` are not given to it, as Cargo
    /// does not give them to the host's build scripts and proc-macros either.
    ///
    /// Unlike the target's build, it does not select the owner to match the main
    /// build's copy of a crate (nothing crosses the boundary between a proc-macro and
    /// the package's code at the type level: the macro only turns tokens into tokens, so
    /// there is no copy to match), only where a path dependency needs it (see below).
    /// The owner crate cannot be used by a proc-macro here at all (see
    /// [`check_owner_use`](Self::check_owner_use)).
    fn build_host_deps(
        &self,
        subcrates: &[Subcrate],
        links: &[Vec<Link>],
        wanted: &[bool],
    ) -> Result<OwnerDeps, Error> {
        if self.host.is_none() {
            return Ok(OwnerDeps::default());
        }
        let mut externals: BTreeMap<&str, String> = BTreeMap::new();
        for (ix, links) in links.iter().enumerate() {
            if !wanted[ix] || !self.is_host_built(&subcrates[ix]) {
                continue;
            }
            for link in links {
                match &link.target {
                    LinkTarget::External(name) => {
                        externals
                            .entry(name)
                            .or_insert_with(|| subcrates[ix].display_name());
                    }
                    LinkTarget::Owner => self.check_owner_use(&subcrates[ix])?,
                    LinkTarget::Subcrate(_) => {}
                }
            }
        }
        if externals.is_empty() {
            return Ok(OwnerDeps::default());
        }
        let lock = self.lock_for_specs();
        let ResolvedExternals {
            mut packages,
            crates,
            links_local_dep,
        } = self.resolve_externals(&externals, &lock)?;
        // A path dependency may be a workspace member, and Cargo then resolves only
        // what the selected members need: a registry crate in the same selection is
        // found only if the owner, which depends on it, is selected too. (Its
        // library is not wanted: see `build_owner_deps`, which does the same.)
        if links_local_dep && !packages.contains(&self.package) {
            packages.push(self.package.clone());
        }
        let target_dir = self.host_deps_target_dir();
        // No dedup against the main build, as the target's does: it has no copy of
        // these crates for the host to match.
        let output = self.run_nested(&packages, &target_dir, links_local_dep, true, &|output| {
            built_nested(output, links_local_dep, &crates)
        })?;
        Ok(self.nested_deps(&output, &target_dir, crates))
    }

    /// The lock file that says which version of a package `::name` stands for when
    /// the dependency graph holds several: the one that governs the nested build (see
    /// [`NestedWorkspace`]).
    fn lock_for_specs(&self) -> Option<(PathBuf, String)> {
        match self.nested_workspace() {
            NestedWorkspace::Shadow(shadow) => shadow.seeds.into_iter().flatten().next(),
            NestedWorkspace::InPlace => find_lock(&self.manifest_dir),
        }
    }

    /// Works out which package each `::name` in `externals` (each with the first
    /// subcrate to use it, for errors) stands for.
    ///
    /// Cargo wants package specs (`-p some-crate`), `::name` is a name the package's
    /// own code uses: `Cargo.toml` says which package that is (a dependency may be
    /// renamed with `package = "…"`), and the lock file which version of it, when
    /// there are several (see [`owner_package`]).
    fn resolve_externals(
        &self,
        externals: &BTreeMap<&str, String>,
        lock: &Option<(PathBuf, String)>,
    ) -> Result<ResolvedExternals, Error> {
        // What this package depends on, as the lock file has it. (Without the lock
        // file, or without this package in it, Cargo itself will say if a name is
        // not a dependency.)
        let locked: Option<Vec<LockedDep>> = lock
            .as_ref()
            .and_then(|(_, text)| locked_dependencies(text, &self.package));
        let declared = std::fs::read_to_string(self.manifest_dir.join("Cargo.toml"))
            .map(|manifest| declared_dependencies(&manifest))
            .unwrap_or_default();
        let mut packages: Vec<String> = Vec::new();
        let mut crates: BTreeMap<String, ExternalCrate> = BTreeMap::new();
        let mut links_local_dep = false;
        for (name, user) in externals {
            let declared_dep = find_declared(&declared, name);
            links_local_dep |= declared_dep.map_or(false, |dep| self.is_local_dep(dep));
            let package = declared_dep.map(|dep| self.declared(dep));
            let resolved =
                owner_package(name, package.as_ref(), locked.as_deref()).map_err(|reason| {
                    Error::new(format!(
                        "subcrate `{}` depends on `::{}`, but {}",
                        user, name, reason
                    ))
                })?;
            // Two names for one package (a rename next to the original) are built once.
            if !packages.contains(&resolved.spec) {
                packages.push(resolved.spec);
            }
            crates.insert(
                name.to_string(),
                ExternalCrate {
                    crate_name: resolved.crate_name,
                    version: resolved.version,
                },
            );
        }
        Ok(ResolvedExternals {
            packages,
            crates,
            links_local_dep,
        })
    }

    /// What the nested build that `output` is the result of — of the crates `crates`
    /// stand for, in `target_dir` — produced, as the subcrates link it.
    fn nested_deps(
        &self,
        output: &std::process::Output,
        target_dir: &Path,
        crates: BTreeMap<String, ExternalCrate>,
    ) -> OwnerDeps {
        let mut deps = parse_artifacts(&String::from_utf8_lossy(&output.stdout));
        deps.externals = crates;
        // The search paths of the crates' build scripts that matter are the ones into
        // what this build produced: a crate backed by subcrates has them in its
        // `OUT_DIR`, and the crates they were built against next to that. A system
        // library's directory (a `-sys` crate's build script) is not something a
        // subcrate needs.
        deps.script_paths.retain(|path| is_inside(path, target_dir));
        self.watch_nested_inputs(&deps);
        deps
    }

    /// Rebuilds the subcrates when the crates they link against change. Cargo does
    /// not rerun this build script for that on its own: it only watches what it is
    /// told to. A dependency from a registry is immutable, but a path dependency is
    /// edited in place, and the resolved versions come from the lock file.
    fn watch_nested_inputs(&self, deps: &OwnerDeps) {
        for source in &deps.sources {
            rerun_if_changed(source);
        }
        match self.nested_workspace() {
            NestedWorkspace::Shadow(shadow) => {
                for (path, _) in shadow.seeds.iter().flatten() {
                    rerun_if_changed(path);
                }
            }
            NestedWorkspace::InPlace => {
                if let Some((path, _)) = find_lock(&self.manifest_dir) {
                    rerun_if_changed(&path);
                }
            }
        }
    }

    /// Runs the nested `cargo build` of `packages` into `target_dir` — for the host
    /// when `host`, else for the target (see [`cargo_args`](Self::cargo_args)) — and
    /// returns its output if `accept` takes it, else the error describing the failure.
    ///
    /// Where it runs, and against which lock file, is [`NestedWorkspace`]: for a
    /// packaged crate it may take several attempts, each with another lock file.
    /// Then the error is the first attempt's.
    fn run_nested(
        &self,
        packages: &[String],
        target_dir: &Path,
        harvest_only: bool,
        host: bool,
        accept: &dyn Fn(&std::process::Output) -> bool,
    ) -> Result<std::process::Output, Error> {
        let mut args = self.cargo_args(packages, target_dir, host);
        let workspace = self.nested_workspace();
        let seeds: Vec<Option<(PathBuf, String)>> = match &workspace {
            NestedWorkspace::InPlace => vec![None],
            NestedWorkspace::Shadow(shadow) => {
                args.push("--manifest-path".into());
                args.push(shadow.manifest.clone().into());
                shadow.seeds.clone()
            }
        };
        let mut first_failure: Option<Error> = None;
        for seed in &seeds {
            if let NestedWorkspace::Shadow(shadow) = &workspace {
                // Start from the lock file of this attempt (or from none).
                let _ = std::fs::remove_file(&shadow.lock);
                if let Some((_, text)) = seed {
                    std::fs::write(&shadow.lock, text)?;
                }
            }
            let output = self.launch_nested(&args, harvest_only, host)?;
            if accept(&output) {
                return Ok(output);
            }
            if first_failure.is_none() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let args: Vec<String> = args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                let mut message = format!(
                    "failed to build the crates the subcrates depend on \
                     (`{} {}` in `{}`):\n{}\n\
                     Every `::name` a `#[subcrate]` uses must be a dependency of this package",
                    self.cargo.display(),
                    args.join(" "),
                    self.manifest_dir.display(),
                    indent(stderr.trim_end())
                );
                // Cargo (checked from 1.78 to current nightly) panics, rather than
                // saying so, when asked to build a package that the manifest's
                // resolution does not activate for the build's platform.
                if stderr.contains("panicked") && stderr.contains("features.rs") {
                    message.push_str(
                        ".\nThe crate has to be a normal dependency that is switched on for \
                         the platform being built (for a proc-macro subcrate, the host's): not \
                         an optional one whose feature is off, not a build-dependency only, \
                         and not one that a `[target.'cfg(…)'.dependencies]` table excludes",
                    );
                }
                first_failure = Some(Error::new(message));
            }
        }
        Err(first_failure.unwrap_or_else(|| Error::new("the nested build was not run")))
    }

    /// Launches `self.cargo` with `args` once, in the package's directory, with the
    /// environment prepared for a nested build (`host`: for the host's).
    fn launch_nested(
        &self,
        args: &[OsString],
        harvest_only: bool,
        host: bool,
    ) -> Result<std::process::Output, Error> {
        let mut cmd = Command::new(&self.cargo);
        let scope = scope_key(&self.manifest_dir);
        cmd.args(args)
            .current_dir(&self.manifest_dir)
            .env(NESTED_ENV, &scope)
            .stdin(Stdio::null());
        // Both are for this package alone (see `scope_key`). Whatever an enclosing
        // nested build left in the environment is another package's, and does not
        // apply to this one.
        if harvest_only {
            cmd.env(HARVEST_ONLY_ENV, &scope);
        } else {
            cmd.env_remove(HARVEST_ONLY_ENV);
        }
        // Cargo sets these for *this* build script only. Left in place they would
        // leak into the build scripts of the packages built below, which then
        // see this package's features and cfgs as their own.
        // And a lint wrapper (`cargo clippy` sets one) is for the check that is
        // under way, not for this build.
        cmd.env_remove("RUSTC_WORKSPACE_WRAPPER");
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("CARGO_FEATURE_") || name.starts_with("CARGO_CFG_") {
                cmd.env_remove(&key);
            }
        }
        if host {
            // These are the *target's* flags (Cargo does not apply `RUSTFLAGS` to
            // what runs on the host when it is given a `--target`), and the build here
            // is the host's.
            cmd.env_remove("CARGO_ENCODED_RUSTFLAGS");
            cmd.env_remove("RUSTFLAGS");
        }
        cmd.output().map_err(|e| {
            Error::new(format!(
                "failed to launch `{}`: {}",
                self.cargo.display(),
                e
            ))
        })
    }

    /// Links the owner dependencies of a *nested* run against the copies the
    /// enclosing owner-dependency build already placed under this run's profile
    /// directory (`main_profile_dir()`), instead of building them again.
    ///
    /// This is the `::owner` path: a subcrate uses the owner crate, so the owner's
    /// library must compile here and its own library subcrates need their owner
    /// dependencies. (A run selected only to unify a path dependency's features
    /// returns earlier from [`run`](Self::run) without reaching this.)
    ///
    /// The enclosing build selects both the dependency and the owner
    /// (see [`build_owner_deps`](Self::build_owner_deps)), so every crate here was
    /// compiled with the owner's feature unification — the same copies the owner's
    /// own library, built in that same enclosing pass, links against. Reusing them
    /// is what keeps a type consistent across the subcrate boundary; building fresh
    /// copies here is exactly what used to reintroduce the mismatch.
    ///
    /// The copies are found by scanning the profile tree rather than a fixed `deps/`,
    /// because Cargo's layout varies — a stable toolchain writes
    /// `deps/lib<crate>-<hash>.rlib`, a recent nightly `build/<pkg>/<hash>/out/…`.
    ///
    /// `crates` maps each `::name` to the library crate it stands for.
    fn owner_deps_from_enclosing(
        &self,
        crates: &BTreeMap<String, ExternalCrate>,
    ) -> Result<OwnerDeps, Error> {
        let base = self.main_profile_dir().ok_or_else(|| {
            Error::new(
                "a subcrate's owner dependency is built as part of the owner's own library, \
                 but the build directory could not be located to link it"
                    .to_string(),
            )
        })?;
        // The enclosing build compiles these crates as selected packages, so Cargo
        // schedules them before the owner's build script (this run). Poll briefly:
        // under parallelism an artifact may not be flushed the instant we look, and
        // it is then being compiled on another job, so the wait is bounded by that
        // compile, never a deadlock.
        let deadline = Instant::now() + Duration::from_secs(10);
        let (rlibs, dirs) = loop {
            let (rlibs, dirs) = scan_build_artifacts(&base);
            let missing = crates
                .values()
                .map(|external| external.crate_name.as_str())
                .find(|name| !rlibs.contains_key(*name));
            match missing {
                None => break (rlibs, dirs),
                Some(name) if Instant::now() >= deadline => {
                    return Err(Error::new(format!(
                        "could not find the owner dependency `{}` under `{}`: it is linked from \
                         the copy the enclosing build compiles, which was expected there",
                        name,
                        base.display()
                    )));
                }
                Some(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        let mut deps = OwnerDeps {
            externals: crates.clone(),
            lib_dirs: dirs,
            ..OwnerDeps::default()
        };
        for external in crates.values() {
            let rlib = rlibs[&external.crate_name].clone();
            let rmeta = rlib.with_extension("rmeta");
            let krate = OwnerCrate {
                rmeta: rmeta.is_file().then(|| rmeta),
                rlib: Some(rlib),
                features: Vec::new(),
                conflict: None,
            };
            deps.crates
                .insert(external.crate_name.clone(), krate.clone());
            if let Some(version) = &external.version {
                deps.versions
                    .insert((external.crate_name.clone(), version.clone()), krate);
            }
        }
        Ok(deps)
    }

    /// Whether a declared dependency is given by path — directly, or inherited from
    /// the workspace — and so may be a member of this package's workspace, the case
    /// that needs the owner in the nested selection (see
    /// [`build_owner_deps`](Self::build_owner_deps)). A registry dependency never is.
    fn is_local_dep(&self, dep: &DeclaredDep) -> bool {
        if dep.path.is_some() {
            return true;
        }
        dep.workspace
            && workspace_dependency(&self.manifest_dir, &dep.key)
                .map_or(false, |inherited| inherited.path.is_some())
    }

    /// What the manifest says a declared dependency `dep` is. The real package is
    /// its `package = "…"` if it is renamed, whatever the workspace declares if it
    /// is inherited (`workspace = true`), else its key. The version, when there are
    /// several, is the version of the package at its `path`, else its `version`
    /// requirement.
    fn declared(&self, dep: &DeclaredDep) -> Declared {
        let inherited = if dep.workspace {
            workspace_dependency(&self.manifest_dir, &dep.key)
        } else {
            None
        };
        let source = inherited.as_ref().unwrap_or(dep);
        let on_disk = dep
            .path
            .as_ref()
            .and_then(|path| {
                std::fs::read_to_string(self.manifest_dir.join(path).join("Cargo.toml")).ok()
            })
            .and_then(|manifest| package_version(&manifest));
        Declared {
            package: source.package.clone().unwrap_or_else(|| source.key.clone()),
            pin: match (on_disk, &source.version) {
                (Some(version), _) => Some(Pin::Exact(version)),
                (None, Some(requirement)) => Some(Pin::Requirement(requirement.clone())),
                (None, None) => None,
            },
        }
    }

    /// The target directory of the nested Cargo build.
    ///
    /// It must be its own directory: the build in progress holds the lock on the
    /// main one. But it is not one per `OUT_DIR`. Cargo gives each configuration of
    /// the package (`cargo build`, `cargo test`, `cargo check`, …) its own
    /// `OUT_DIR`, and a nested build into each would compile the same crates again
    /// and again — the owner's build-dependencies among them, when building the
    /// owner. The nested build does not depend on the configuration: it does not
    /// see this one's features or cfgs, and resolves from the package's
    /// `Cargo.toml`. So they all share
    /// `<profile directory>/subcrate-owner-deps/<package>/<compiler>`.
    ///
    /// That is per package, per profile and target (the profile directory is the
    /// one `OUT_DIR` sits in — `target/debug`, `target/<triple>/release`), and per
    /// compiler. Per compiler because Cargo links the crates it builds for the
    /// packages asked for to one name (`libleaf.rlib`, which the subcrates are
    /// compiled against), whichever compiler built them; two compilers sharing a
    /// directory would swap them under each other. Two builds that meet here wait on
    /// this directory's lock, which is not the main target directory's, so they
    /// cannot deadlock.
    ///
    /// Without a `build` directory above `OUT_DIR` to locate it by, the nested build
    /// gets a directory inside `OUT_DIR` itself.
    fn owner_deps_target_dir(&self) -> PathBuf {
        shared_target_dir(
            &self.out_dir,
            &self.owner_deps_dir_key(),
            rustc_key(&self.rustc).as_deref(),
        )
    }

    /// The target directory of the nested build of the crates the proc-macro
    /// subcrates use when cross-compiling: the target's, with `-host` after the
    /// compiler. (Its own directory, so the two builds never share a lock or an
    /// artifact name.)
    fn host_deps_target_dir(&self) -> PathBuf {
        let compiler = match rustc_key(&self.rustc) {
            Some(key) => format!("{}-host", key),
            None => "host".to_string(),
        };
        shared_target_dir(&self.out_dir, &self.owner_deps_dir_key(), Some(&compiler))
    }

    /// The directory component naming the nested build: the opt-in
    /// [`owner_deps_key`](Self::owner_deps_key) when set (sanitized to one path
    /// component so it cannot escape), else the package name (the per-package
    /// default).
    fn owner_deps_dir_key(&self) -> String {
        match &self.owner_deps_key {
            Some(key) => sanitize_dir_component(key),
            None => self.package.clone(),
        }
    }

    /// The main build's profile directory — `target/debug`, `target/<triple>/release`
    /// — the one `OUT_DIR` sits under (the directory holding `build`). The owner's
    /// own dependency artifacts live beneath it, and the nested build's target
    /// directory ([`owner_deps_target_dir`](Self::owner_deps_target_dir)) sits
    /// beneath it too, under `subcrate-owner-deps`. `None` when the layout is not the
    /// usual one, in which case the disk-reclaiming step is simply skipped.
    fn main_profile_dir(&self) -> Option<PathBuf> {
        self.out_dir
            .ancestors()
            .find(|dir| dir.file_name().map_or(false, |name| name == "build"))
            .and_then(Path::parent)
            .map(Path::to_path_buf)
    }

    /// The arguments of the nested `cargo build` of `packages` into `target_dir`.
    ///
    /// * `--lib` builds just the library of each package. For the owner crate this
    ///   leaves out the binaries (which may hold subcrates that use the owner).
    /// * `--offline` keeps the nested Cargo from contending for the package-cache
    ///   lock the outer one may hold.
    /// * `--release` matches the outer profile when it is `release`; any other
    ///   profile, custom ones included, is built as `dev` (a build script is only
    ///   told `debug` or `release`).
    /// * `--target` is passed only when cross-compiling: otherwise the build would
    ///   be for an explicit target, which Cargo gives a different identity. It is
    ///   the target's triple, or — when `host` asks for the build of a proc-macro
    ///   subcrate's crates — the host's.
    ///
    /// Cargo's own diagnostics are rendered to stderr (shown if the build fails),
    /// leaving only the artifact messages on stdout as JSON.
    fn cargo_args(&self, packages: &[String], target_dir: &Path, host: bool) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec!["build".into()];
        for package in packages {
            args.push("-p".into());
            args.push(package.into());
        }
        args.push("--lib".into());
        args.push("--message-format=json-render-diagnostics".into());
        args.push("--offline".into());
        args.push("--target-dir".into());
        args.push(target_dir.into());
        if self.profile == "release" {
            args.push("--release".into());
        }
        let triple = if host { &self.host } else { &self.target };
        if let Some(triple) = triple {
            args.push("--target".into());
            args.push(triple.into());
        }
        args
    }

    /// Where the nested builds run — worked out once, and cached.
    fn nested_workspace(&self) -> NestedWorkspace {
        if let Some(workspace) = &*self.nested_workspace.borrow() {
            return workspace.clone();
        }
        let workspace = self
            .shadow_workspace()
            .map_or(NestedWorkspace::InPlace, NestedWorkspace::Shadow);
        *self.nested_workspace.borrow_mut() = Some(workspace.clone());
        workspace
    }

    /// Sets up the private workspace of a packaged crate (see [`NestedWorkspace`]), or
    /// `None` when the package is not one, or it cannot stand on its own in one, or
    /// the file system will not let it be set up: the nested builds then run in the
    /// package's directory, as they always did.
    fn shadow_workspace(&self) -> Option<Shadow> {
        if !is_packaged_dir(&self.manifest_dir) {
            return None;
        }
        let manifest = std::fs::read_to_string(self.manifest_dir.join("Cargo.toml")).ok()?;
        // A packaged manifest has neither a workspace nor inherited fields nor a
        // relative path dependency: it was written to stand alone. One that does is
        // not really packaged (a checkout that happens to be under a registry's
        // directories), and needs the workspace it sits in.
        if mentions_workspace(&manifest)
            || declared_dependencies(&manifest)
                .iter()
                .any(|dep| dep.workspace || dep.path.as_deref().map_or(false, relative_path))
        {
            return None;
        }
        let root = shared_target_dir(&self.out_dir, &self.owner_deps_dir_key(), None)
            .join(format!("ws-{:016x}", path_hash(&self.out_dir)));
        std::fs::create_dir_all(&root).ok()?;
        link_dir(&self.manifest_dir, &root.join(SHADOW_MEMBER)).ok()?;

        let seeds = self.lock_candidates();
        // `cargo package` of several packages at once verifies each against a local
        // overlay of the others, which only Cargo's own process knows of; a package
        // that depends on one of them finds it nowhere else. Its extracted source is
        // beside this package's, though, so a patch can point to it.
        let patches = match seeds.iter().flatten().next() {
            Some((_, lock)) => self.overlay_patches(lock),
            None => Vec::new(),
        };
        let root_manifest = root.join("Cargo.toml");
        let text = shadow_manifest(&manifest, &detect_edition(&self.manifest_dir), &patches);
        if std::fs::read_to_string(&root_manifest).ok().as_deref() != Some(text.as_str()) {
            std::fs::write(&root_manifest, text).ok()?;
        }
        Some(Shadow {
            manifest: root_manifest,
            lock: root.join("Cargo.lock"),
            seeds,
        })
    }

    /// The lock files the nested build of a packaged crate is tried against, in turn
    /// (the last always: none, so Cargo resolves afresh from what the local cache
    /// holds).
    ///
    /// 1. The lock file of the workspace being built, if this package is a dependency
    ///    in it: the versions the outer build resolved, so the crates the subcrates
    ///    link are the ones the package itself is compiled against. It is found above
    ///    `OUT_DIR` (in the usual layout, the `target/` directory sits in the
    ///    workspace); Cargo does not say where it is.
    /// 2. The package's own `Cargo.lock`, which is the right one when the package is
    ///    itself what is being built (`cargo package`'s verification, docs.rs, `cargo
    ///    install --locked`). Otherwise it may name versions that are not in the cache.
    fn lock_candidates(&self) -> Vec<Option<(PathBuf, String)>> {
        let mut candidates: Vec<Option<(PathBuf, String)>> = Vec::new();
        for dir in self.out_dir.ancestors() {
            let path = dir.join("Cargo.lock");
            if let Ok(text) = std::fs::read_to_string(&path) {
                if lock_has_dependency(&text, &self.package, &self.package_version) {
                    candidates.push(Some((path, text)));
                    break;
                }
            }
        }
        let own = self.manifest_dir.join("Cargo.lock");
        if let Ok(text) = std::fs::read_to_string(&own) {
            if !candidates.iter().flatten().any(|(_, other)| *other == text) {
                candidates.push(Some((own, text)));
            }
        }
        candidates.push(None);
        candidates
    }

    /// The `[patch.crates-io]` entries that point the registry packages of the lock
    /// file `lock` at their extracted sources, when this package is being verified by
    /// `cargo package` against its siblings: the extracted package is in
    /// `target/package/`, next to `tmp-registry` (Cargo's overlay) and the siblings'.
    fn overlay_patches(&self, lock: &str) -> Vec<(String, PathBuf)> {
        let dir = match self.manifest_dir.parent() {
            Some(dir)
                if dir.file_name().map_or(false, |name| name == "package")
                    && dir.join("tmp-registry").is_dir() =>
            {
                dir
            }
            _ => return Vec::new(),
        };
        let mut patches: Vec<(String, PathBuf)> = Vec::new();
        for package in locked_packages(lock) {
            let from_crates_io = package.source.as_deref().map_or(false, |source| {
                source == "registry+https://github.com/rust-lang/crates.io-index"
                    || source.starts_with("sparse+https://index.crates.io")
            });
            let sibling = dir.join(format!("{}-{}", package.name, package.version));
            if from_crates_io
                && package.name != self.package
                && sibling.join("Cargo.toml").is_file()
                && !patches.iter().any(|(name, _)| *name == package.name)
            {
                patches.push((package.name, sibling));
            }
        }
        patches
    }

    /// The edition `sub` is compiled with: its own override, else the package's.
    fn edition_of<'a>(&'a self, sub: &'a Subcrate) -> &'a str {
        sub.edition.as_deref().unwrap_or(&self.edition)
    }

    /// Prepares `sub` for compilation, doing the parts that need `self` and syn
    /// (whose tokens are not `Send`) on the calling thread: render the source,
    /// build the `rustc` arguments, and decide reuse. Returns either that it is
    /// already up to date (with any cached warnings to re-emit) or an owned,
    /// `Send` [`CompileJob`] a worker thread runs with [`run_compile_job`].
    fn prepare_subcrate(
        &self,
        sub: &Subcrate,
        deps: &[ExternDep],
        out_dir: &Path,
    ) -> Result<Prepared, Error> {
        let args = self.rustc_args(sub, deps, out_dir);
        // `mappable` off for a body inlined from several files (`mod foo;`): its
        // tokens span more than `source_file`, so leave diagnostics as `<anon>`.
        let (source, line_map) = render_source(
            &sub.inner_attrs,
            deps,
            &sub.items,
            !sub.inlined_files,
            sub.proc_macro,
        );

        // Reuse decision. The crate *name* is already a hash of the body as
        // written. The stamp records more: the exact `rustc` configuration, and
        // the fully rendered source. The rendered source is in there so that
        // editing a file inlined via `mod foo;` — whose content is not in the name
        // — still forces a rebuild. We rebuild unless all of it matches. The name
        // itself stays config-independent, so the proc macro can recompute it.
        //
        // The configuration includes each `--extern <internal name>=<artifact>`.
        // A dependency's name embeds that dependency's own content hash, so a
        // changed dependency changes this subcrate's fingerprint and recompiles it
        // too, even though this subcrate's own name is unchanged. (The injected
        // `extern crate` of each dependency is part of the rendered source, so it is
        // in the fingerprint, but it is a build-time detail and not in the name.)
        //
        // An owner dependency's file name is *not* content-addressed: Cargo's hash
        // covers its package and features, not its source. So its files' sizes and
        // modification times are folded in instead, and a rebuilt dependency then
        // recompiles the subcrates linked against it. The same goes for a sibling
        // subcrate built on one, whose name does not cover that either. (The
        // fingerprint reads these files *after* the dependency's wave has built, so
        // the stamps are current.)
        let mut fingerprint = format!("{}\u{1f}{}", args.join("\u{1f}"), source);
        for path in deps
            .iter()
            .filter(|dep| dep.owner_linked)
            .flat_map(|dep| &dep.paths)
        {
            fingerprint.push('\u{1f}');
            fingerprint.push_str(&file_stamp(path));
        }
        let stamp = out_dir.join(format!("{}.stamp", sub.internal));
        let warnings_file = out_dir.join(format!("{}.warnings", sub.internal));
        if let Ok(previous) = std::fs::read_to_string(&stamp) {
            if previous == fingerprint {
                // Reuse: rustc is not run, so re-emit the warnings it printed when
                // it last was (kept beside the stamp), so they show on every build
                // as Cargo's own do, not only when the subcrate is recompiled.
                return Ok(Prepared::Reused(
                    std::fs::read_to_string(&warnings_file).ok(),
                ));
            }
        }
        Ok(Prepared::Compile(Box::new(CompileJob {
            rustc: self.rustc.clone(),
            args,
            source,
            out_dir: out_dir.to_path_buf(),
            stamp,
            fingerprint,
            warnings_file,
            display_name: sub.display_name(),
            proc_macro: sub.proc_macro,
            line_map,
            source_file: sub.source_file.display().to_string(),
            ui: sub.ui.clone(),
        })))
    }

    /// Compiles the wanted subcrates in dependency order, independent ones in
    /// parallel: each step's "wave" is every subcrate whose sibling dependencies
    /// have all succeeded, and its members are compiled on worker threads (capped
    /// by Cargo's `NUM_JOBS`). The syn work for each is done on this thread first,
    /// since syn tokens are not `Send`. It does not stop at the first failure: a
    /// subcrate whose dependency failed is left unbuilt, and if any failed the
    /// returned error lists them all, so one build reports every broken subcrate.
    fn compile_all(
        &self,
        subcrates: &[Subcrate],
        links: &[Vec<Link>],
        needs: &[Vec<usize>],
        wanted: &[bool],
        subcrates_dir: &Path,
        owner_linked: &[bool],
    ) -> Result<Vec<bool>, Error> {
        let n = subcrates.len();
        let mut status = vec![Status::Pending; n];
        let mut errors: Vec<String> = Vec::new();
        let mut skipped: Vec<usize> = Vec::new();
        // One flag per subcrate: skipped because its `#[rustversion]` gate did not
        // build on this toolchain. Returned so the manifest records it as skipped
        // (the macro then expands it away). See [`is_rustversion_gate`].
        let mut gate_skipped = vec![false; n];
        let jobs = std::env::var("NUM_JOBS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&j| j > 0)
            .unwrap_or(4);

        loop {
            let mut progressed = false;

            // A subcrate a failed (or already-skipped) dependency makes unbuildable
            // is itself skipped — recorded, and marked `Failed` so its own
            // dependents skip in turn.
            for ix in 0..n {
                if !wanted[ix] || status[ix] != Status::Pending {
                    continue;
                }
                if needs[ix]
                    .iter()
                    .any(|&d| wanted[d] && status[d] == Status::Failed)
                {
                    status[ix] = Status::Failed;
                    skipped.push(ix);
                    progressed = true;
                }
            }

            // A subcrate that needs a gate-skipped dependency can't build on this
            // toolchain either. If it is itself gated, skip it too (quietly); if
            // not, it is a configuration error — it must be gated the same way, or
            // not depend on a gated subcrate. (The `Failed` sweep above runs first,
            // so a real dependency error always wins over this.)
            for ix in 0..n {
                if !wanted[ix] || status[ix] != Status::Pending {
                    continue;
                }
                if let Some(&d) = needs[ix]
                    .iter()
                    .find(|&&d| wanted[d] && status[d] == Status::Skipped)
                {
                    if subcrates[ix].gated {
                        status[ix] = Status::Skipped;
                        gate_skipped[ix] = true;
                        write_skip_reason(
                            &self.out_dir,
                            &subcrates[ix].internal,
                            &format!(
                                "depends on `{}`, which was skipped on this toolchain",
                                subcrates[d].display_name()
                            ),
                        );
                    } else {
                        status[ix] = Status::Failed;
                        errors.push(format!(
                            "`{}` depends on `{}`, which carries a `#[rustversion]` gate \
                             and was not built on this toolchain. Gate `{}` the same way, \
                             or drop the dependency.",
                            subcrates[ix].display_name(),
                            subcrates[d].display_name(),
                            subcrates[ix].display_name(),
                        ));
                    }
                    progressed = true;
                }
            }

            // The ready wave: pending, wanted, every wanted dependency done. (A
            // wanted subcrate never needs a non-wanted one — the wanted set is
            // closed over `needs` — so non-wanted ones never block it.)
            let wave: Vec<usize> = (0..n)
                .filter(|&ix| {
                    wanted[ix]
                        && status[ix] == Status::Pending
                        && needs[ix]
                            .iter()
                            .all(|&d| !wanted[d] || status[d] == Status::Done)
                })
                .collect();
            if wave.is_empty() {
                // Nothing ready: either everything is handled, or (defensively — a
                // cycle is already rejected) progress stalled.
                if progressed {
                    continue;
                }
                break;
            }

            // Prepare each member on this thread (the syn work), turning it into a
            // runnable job; reuse and prepare errors are handled here.
            let mut batch: Vec<(usize, CompileJob)> = Vec::new();
            for &ix in &wave {
                let deps = match self.extern_deps(
                    subcrates,
                    ix,
                    &links[ix],
                    subcrates_dir,
                    owner_linked,
                ) {
                    Ok(deps) => deps,
                    Err(e) => {
                        self.note_compile_failure(
                            &subcrates[ix],
                            e.to_string(),
                            &mut status[ix],
                            &mut errors,
                            &mut gate_skipped[ix],
                        );
                        continue;
                    }
                };
                match self.prepare_subcrate(&subcrates[ix], &deps, subcrates_dir) {
                    Ok(Prepared::Reused(cached)) => {
                        if let Some(text) = cached {
                            emit_subcrate_warnings(&subcrates[ix], &text);
                        }
                        status[ix] = Status::Done;
                    }
                    Ok(Prepared::Compile(job)) => batch.push((ix, *job)),
                    Err(e) => {
                        self.note_compile_failure(
                            &subcrates[ix],
                            e.to_string(),
                            &mut status[ix],
                            &mut errors,
                            &mut gate_skipped[ix],
                        );
                    }
                }
            }

            // Run the jobs on worker threads, at most `jobs` at a time.
            let mut rest = batch;
            while !rest.is_empty() {
                let take = rest.len().min(jobs);
                let now: Vec<(usize, CompileJob)> = rest.drain(..take).collect();
                let handles: Vec<(usize, JobHandle)> = now
                    .into_iter()
                    .map(|(ix, job)| (ix, std::thread::spawn(move || run_compile_job(&job))))
                    .collect();
                for (ix, handle) in handles {
                    let result = handle.join().unwrap_or_else(|_| {
                        Err(format!(
                            "compiling subcrate `{}` panicked",
                            subcrates[ix].display_name()
                        ))
                    });
                    match result {
                        Ok(warnings) => {
                            if let Some(text) = warnings {
                                emit_subcrate_warnings(&subcrates[ix], &text);
                            }
                            status[ix] = Status::Done;
                        }
                        Err(message) => {
                            self.note_compile_failure(
                                &subcrates[ix],
                                message,
                                &mut status[ix],
                                &mut errors,
                                &mut gate_skipped[ix],
                            );
                        }
                    }
                }
            }
        }

        if errors.is_empty() {
            return Ok(gate_skipped);
        }
        let mut message = if errors.len() == 1 {
            errors.remove(0)
        } else {
            format!(
                "{} subcrates failed to compile:\n\n{}",
                errors.len(),
                errors.join("\n\n")
            )
        };
        if !skipped.is_empty() {
            let names: Vec<String> = skipped
                .iter()
                .map(|&ix| format!("`{}`", subcrates[ix].display_name()))
                .collect();
            message.push_str(&format!(
                "\n\nnot built, because a dependency above failed: {}",
                names.join(", ")
            ));
        }
        Err(Error::new(message))
    }

    /// Records that compiling `sub` failed. For a `#[rustversion]`-gated subcrate
    /// this is "not available on this toolchain": it becomes [`Status::Skipped`],
    /// the compiler's error is kept for the macro (see [`write_skip_reason`]), and
    /// the build does not fail on its account. For any other subcrate it is a real
    /// error, added to `errors`, and the subcrate is [`Status::Failed`].
    fn note_compile_failure(
        &self,
        sub: &Subcrate,
        error: String,
        status: &mut Status,
        errors: &mut Vec<String>,
        gate_skipped: &mut bool,
    ) {
        if sub.gated {
            *status = Status::Skipped;
            *gate_skipped = true;
            write_skip_reason(&self.out_dir, &sub.internal, &error);
        } else {
            *status = Status::Failed;
            errors.push(error);
        }
    }

    /// Builds the `rustc` argument list (everything except the `-` input and
    /// `--out-dir`) used to compile `sub`, linking the dependency artifacts
    /// `deps` found in `subcrates_dir`. This is also hashed into the reuse
    /// stamp, so it must deterministically reflect the full configuration.
    fn rustc_args(&self, sub: &Subcrate, deps: &[ExternDep], subcrates_dir: &Path) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();
        args.push("--edition".into());
        // A `#[subcrate(edition = "…")]` override wins over the package edition.
        args.push(self.edition_of(sub).to_string());
        args.push("--crate-name".into());
        args.push(sub.internal.clone());
        args.push("-C".into());
        args.push(format!("opt-level={}", self.opt_level));

        // Cast the owner's enabled feature cfgs onto the subcrate (applies to
        // both library and proc-macro subcrates — features are logical, not
        // target-specific).
        for cfg in &self.feature_cfgs {
            args.push("--cfg".into());
            args.push(cfg.clone());
        }

        if sub.proc_macro {
            // Proc-macro crates are host artifacts: they must be built with the
            // host's default configuration, so the target's panic strategy,
            // `--target` and target `RUSTFLAGS` deliberately do NOT apply here.
            args.push("--crate-type".into());
            args.push("proc-macro".into());
            args.push("--extern".into());
            args.push("proc_macro".into());
        } else {
            // Library subcrates are part of the final artifact for the target,
            // so they mirror the owning package's target configuration.
            args.push("--crate-type".into());
            args.push("lib".into());
            args.push("--emit".into());
            args.push("metadata,link".into());
            if let Some(target) = &self.target {
                args.push("--target".into());
                args.push(target.clone());
            }
            if let Some(panic) = &self.panic {
                args.push("-C".into());
                args.push(format!("panic={}", panic));
            }
            args.push("-C".into());
            args.push(format!(
                "debug-assertions={}",
                if self.debug_assertions { "on" } else { "off" }
            ));
            // Honour RUSTFLAGS / `[build] rustflags` / target flags just as
            // Cargo would for the package itself (target features, `--cfg`,
            // lint levels, codegen options, …).
            args.extend(self.rustflags.iter().cloned());
        }

        // Link the sibling subcrates this one depends on, each as
        // `--extern <its internal crate name>=<artifact>`; `render_source` then
        // makes it visible under its alias. The directory is also a search path so
        // rustc can find a dependency's *own* dependencies (they are recorded in
        // its metadata by crate name).
        if !deps.is_empty() {
            args.push("-L".into());
            args.push(format!("dependency={}", subcrates_dir.display()));
        }
        // The same goes for the owner's dependencies, whose own dependencies sit in
        // directories of their own, and for whatever is built on them. (The owner
        // crate also needs `subcrates_dir`, above, for the subcrates its library is
        // made of.)
        if deps.iter().any(|dep| dep.owner_linked) {
            for dir in self.dependency_dirs() {
                args.push("-L".into());
                args.push(format!("dependency={}", dir.display()));
            }
            // A dependency that is itself backed by subcrates names them in its
            // metadata, and its build script put them where only it looks: Cargo
            // forwards that search path to its own compilations, so forward it to
            // these too. (Not to the package's own compilations: Cargo already
            // gives them the main build's copy, and two copies of one crate on a
            // search path are an error.)
            for dir in self.script_dirs() {
                args.push("-L".into());
                args.push(format!("dependency={}", dir.display()));
            }
        }
        let mut linked: HashSet<&str> = HashSet::new();
        for dep in deps {
            // One crate under two aliases is still linked once. It may take several
            // files (an external crate's rlib *and* rmeta), all under the one name.
            if linked.insert(&dep.internal) {
                for path in &dep.paths {
                    args.push("--extern".into());
                    args.push(format!("{}={}", dep.internal, path.display()));
                }
            }
        }

        args.extend(self.extra_rustc_args.iter().cloned());

        // A UI test asks rustc for machine-readable diagnostics so the build
        // helper can check the messages and underlined spans (see `run_ui_check`).
        // The `rendered` field of each JSON diagnostic still carries the normal
        // human text, which is what is shown if an expectation is not met.
        // `--color=never` keeps that text free of ANSI escapes.
        //
        // `--cap-lints warn` caps every lint at `warn` for the UI test only, so a
        // `should_warn` assertion still works when the owner builds with
        // `-D warnings` (which otherwise promotes the warning to an error and makes
        // the subcrate fail to compile). It cannot hide a genuine compile error, so
        // `should_error` assertions are unaffected. Non-UI subcrates keep inheriting
        // the owner's lint flags, since a real subcrate should honour them.
        if sub.ui.is_ui_test() {
            args.push("--error-format=json".into());
            args.push("--color=never".into());
            args.push("--cap-lints".into());
            args.push("warn".into());
        }

        args
    }
}

/// The `::name` crates a nested build is asked for, resolved by
/// [`Config::resolve_externals`].
struct ResolvedExternals {
    /// The package specs to build (`-p …`).
    packages: Vec<String>,
    /// What each `::name` stands for.
    crates: BTreeMap<String, ExternalCrate>,
    /// Whether any is given by path, and so may be a member of the workspace.
    links_local_dep: bool,
}

/// Whether the nested build whose output is `output` built what was asked for. A
/// harvest-only build deliberately leaves the owner's own library uncompiled, so Cargo
/// exits non-zero; that is expected as long as every crate the subcrates link was
/// built (each is reported before the owner's library, which depends on it, so it is
/// in the output here). Any other failure — or a dependency that genuinely did not
/// build — is a real error.
fn built_nested(
    output: &std::process::Output,
    harvest_only: bool,
    crates: &BTreeMap<String, ExternalCrate>,
) -> bool {
    output.status.success()
        || (harvest_only && {
            let built = parse_artifacts(&String::from_utf8_lossy(&output.stdout));
            crates
                .values()
                .all(|external| built.crates.contains_key(&external.crate_name))
        })
}

/// The name of the member of a [`Shadow`] workspace that is the package.
const SHADOW_MEMBER: &str = "pkg";

/// Where, and against which lock file, the nested `cargo build` runs.
///
/// Cargo takes a package's workspace, and so its `Cargo.lock`, from the directory it
/// runs in. For a package that is a member of the workspace being built, or a
/// checkout, that is right: the same versions the outer build resolved. For a package
/// *built from a registry* (or a git checkout) it is wrong in two ways: the package's
/// own `Cargo.lock` — the one it was published with — names versions that need not be
/// in the local cache (the nested build is `--offline`), so the build fails for want
/// of a download; and Cargo rewrites that lock file, in the registry's source
/// directory, which is nobody's to write (and may be read-only).
#[derive(Debug, Clone)]
enum NestedWorkspace {
    /// In the package's own directory, against the lock file of the workspace it is
    /// in.
    InPlace,
    /// In a private workspace of the build script's own: see [`Shadow`].
    Shadow(Shadow),
}

/// A private workspace around a packaged crate.
///
/// It sits in the build's profile directory, next to the nested target directory: a
/// `Cargo.toml` with a `[workspace]` whose only member, `pkg`, is a symbolic link to
/// the package's directory, and its own `Cargo.lock`. The nested `cargo build` runs
/// with `--manifest-path` of that workspace, from the package's directory still (so
/// the Cargo configuration files, which Cargo finds from the current directory,
/// stay in reach). The package's own `Cargo.lock` is never read by Cargo nor written.
///
/// The lock file the nested build starts from is, preferably, the versions the outer
/// build resolved: see [`Config::lock_candidates`]. The package is a member here
/// instead of a root, so the `[profile]` and `[patch]` tables of its manifest are
/// ignored — as they are when Cargo builds it as a dependency.
#[derive(Debug, Clone)]
struct Shadow {
    /// The workspace's manifest, for `--manifest-path`.
    manifest: PathBuf,
    /// Its lock file.
    lock: PathBuf,
    /// The lock files to start a nested build from, one after the other until one
    /// builds (`None`: no lock file).
    seeds: Vec<Option<(PathBuf, String)>>,
}

/// Whether the package at `dir` was packaged: extracted from a `.crate` file (by a
/// registry, by `cargo package`'s verification, by `cargo vendor`) or checked out of
/// a git repository by Cargo, rather than a source tree somebody works in. Every
/// packaged crate holds `Cargo.toml.orig`, the manifest as written.
fn is_packaged_dir(dir: &Path) -> bool {
    if dir.join("Cargo.toml.orig").is_file() {
        return true;
    }
    let names: Vec<&std::ffi::OsStr> = dir.components().map(|part| part.as_os_str()).collect();
    names.windows(2).any(|pair| {
        (pair[0] == "registry" && pair[1] == "src") || (pair[0] == "git" && pair[1] == "checkouts")
    })
}

/// Whether `path` is relative: a path dependency given so is relative to the
/// manifest, and would not find its target from the private workspace's link.
fn relative_path(path: &str) -> bool {
    !Path::new(path).is_absolute()
}

/// Whether a manifest mentions a workspace — a `[workspace]` table, or a field or
/// dependency taken from one (`version.workspace = true`, `{ workspace = true }`).
fn mentions_workspace(manifest: &str) -> bool {
    manifest
        .lines()
        .map(|line| strip_toml_comment(line).trim())
        .any(|line| {
            line.starts_with("[workspace")
                || line.contains(".workspace")
                || line.contains("workspace =")
                || line.contains("workspace=")
        })
}

/// A hash of a path, for a directory name that is stable for it.
fn path_hash(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

/// Makes `link` a symbolic link to the directory `target`, replacing a link to
/// another place.
fn link_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    if std::fs::read_link(link).map_or(false, |existing| existing == target) {
        return Ok(());
    }
    // Whatever is there (a stale link) goes; a real directory is not removable here
    // unless it is empty, which is as it should be.
    let _ = std::fs::remove_file(link);
    let _ = std::fs::remove_dir(link);
    symlink_dir(target, link)
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(not(any(unix, windows)))]
fn symlink_dir(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        "symbolic links are not supported here",
    ))
}

/// The `Cargo.toml` of a [`Shadow`] workspace: the member, the resolver the package's
/// own manifest would have as a workspace root (its `resolver`, else by edition), and
/// the `patches` as `[patch.crates-io]`.
fn shadow_manifest(package: &str, edition: &str, patches: &[(String, PathBuf)]) -> String {
    let mut explicit = None;
    let mut in_package = false;
    for raw in package.lines() {
        let line = strip_toml_comment(raw).trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
        } else if in_package {
            if let Some((key, value)) = split_key_value(line) {
                if key.trim() == "resolver" {
                    explicit = toml_string_value(value);
                }
            }
        }
    }
    let resolver = match explicit.as_deref() {
        Some(resolver @ ("1" | "2" | "3")) => resolver,
        _ => match edition {
            "2015" | "2018" => "1",
            "2021" => "2",
            _ => "3",
        },
    };
    let mut text = format!(
        "# Written by subcrate's build helper: a workspace around the packaged crate, so the\n\
         # nested build has a Cargo.lock of its own and leaves the package's alone.\n\
         [workspace]\nmembers = [\"{}\"]\nresolver = \"{}\"\n",
        SHADOW_MEMBER, resolver
    );
    if !patches.is_empty() {
        text.push_str("\n[patch.crates-io]\n");
        for (name, path) in patches {
            text.push_str(&format!("{} = {{ path = {} }}\n", name, toml_quote(path)));
        }
    }
    text
}

/// A path as a TOML basic string.
fn toml_quote(path: &Path) -> String {
    let mut out = String::from("\"");
    for c in path.to_string_lossy().chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The directory the nested builds of `package` share (see
/// [`Config::owner_deps_target_dir`]): `subcrate-owner-deps/<package>` — and
/// `/<compiler>`, if known — in the profile directory above `out_dir`.
fn shared_target_dir(out_dir: &Path, package: &str, compiler: Option<&str>) -> PathBuf {
    let profile_dir = out_dir
        .ancestors()
        .find(|dir| dir.file_name().map_or(false, |name| name == "build"))
        .and_then(Path::parent);
    match profile_dir {
        Some(dir) => {
            let dir = dir.join("subcrate-owner-deps").join(package);
            match compiler {
                Some(compiler) => dir.join(compiler),
                None => dir,
            }
        }
        None => out_dir.join("owner-deps-target"),
    }
}

/// Makes a user-supplied key safe as a single path component: ASCII alphanumerics
/// and `-`/`_` are kept, everything else (path separators and `.` among them, so no
/// `..` traversal) becomes `_`. An all-stripped key would be empty, so `_` stands
/// in to keep it a valid directory name.
fn sanitize_dir_component(key: &str) -> String {
    let mut out: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// A best-effort cross-process advisory lock for the nested owner-dependency
/// directory, held around the [`dedup_against_main`] step. When several build
/// scripts share one such directory ([`Config::owner_deps_key`], or a package's own
/// parallel `build`/`test`), it stops them rewriting each other's hard links at
/// once. It is a lock *file* created with `create_new`; [`acquire`](Self::acquire)
/// spins briefly for it and steals one a crashed build left behind (older than a
/// timeout) so a crash cannot wedge other builds, and `Drop` removes it. If it
/// cannot be taken the dedup still runs — it is only disk reclamation, and each
/// link is made atomically regardless.
struct DedupLock {
    path: PathBuf,
}

impl DedupLock {
    fn acquire(dir: &Path) -> Option<DedupLock> {
        Self::acquire_with(dir, Duration::from_secs(10), Duration::from_secs(30))
    }

    fn acquire_with(dir: &Path, wait: Duration, stale: Duration) -> Option<DedupLock> {
        let path = dir.join(".subcrate-dedup.lock");
        let deadline = Instant::now() + wait;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Some(DedupLock { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Steal a lock a crashed build left behind (older than `stale`).
                    let aged = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .map_or(false, |age| age > stale);
                    if aged {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                // The directory is gone or unwritable: skip locking.
                Err(_) => return None,
            }
        }
    }
}

impl Drop for DedupLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The compiled-output extensions worth deduplicating — the large artifacts.
/// Cargo names each by a hash of its content and configuration, so two files of
/// one name hold one content.
const ARTIFACT_EXTENSIONS: &[&str] = &["rlib", "rmeta", "so", "dylib", "dll"];

fn is_artifact(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map_or(false, |ext| ARTIFACT_EXTENSIONS.contains(&ext))
}

/// Replace every artifact under `side` that is byte-identical to one in the main
/// profile directory `main` with a hard link to the main copy, reclaiming the disk
/// the nested build spent re-storing the owner's dependency chain. See the call
/// site for why this is sound. Best effort throughout: an I/O error on any one
/// file leaves that duplicate in place and moves on.
fn dedup_against_main(side: &Path, main: &Path) {
    // Index the main target's artifacts by file name — content-addressed, so the
    // name is a near-unique key. Skip the whole `subcrate-owner-deps` subtree (the
    // nested builds' target directories, `side` among them, live there): its copies
    // are duplicates themselves, and a side artifact should link to the canonical
    // copy in the main target, not to another package's duplicate.
    let mut by_name: BTreeMap<OsString, PathBuf> = BTreeMap::new();
    index_artifacts(main, &main.join("subcrate-owner-deps"), &mut by_name);
    link_duplicates(side, &by_name);
}

/// Walk `dir` (skipping the subtree `skip`) and record each artifact file under
/// its file name. The first file of a given name wins; a second is all but
/// impossible under content addressing, and either copy would serve.
fn index_artifacts(dir: &Path, skip: &Path, by_name: &mut BTreeMap<OsString, PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == skip {
            continue;
        }
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => index_artifacts(&path, skip, by_name),
            Ok(ft) if ft.is_file() && is_artifact(&path) => {
                by_name.entry(entry.file_name()).or_insert(path);
            }
            _ => {}
        }
    }
}

/// Walk `dir` and hard-link each artifact whose name matches one in `by_name`.
fn link_duplicates(dir: &Path, by_name: &BTreeMap<OsString, PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => link_duplicates(&path, by_name),
            Ok(ft) if ft.is_file() && is_artifact(&path) => {
                if let Some(main) = by_name.get(&entry.file_name()) {
                    link_if_identical(&path, main);
                }
            }
            _ => {}
        }
    }
}

/// Replace `side` with a hard link to `main` when the two hold the same bytes and
/// are not already one file. The link is made under a temporary name and renamed
/// over `side`, so an interruption can never leave `side` missing (a half-done
/// step just leaves the original, which the next build reuses).
fn link_if_identical(side: &Path, main: &Path) {
    if same_file(side, main) || !same_contents(side, main) {
        return;
    }
    // A temporary name unique to this file and this process, so two builds
    // deduplicating the same directory at once never clobber each other's
    // in-progress link (they would only ever race to the identical result, but
    // sharing one temp name would make them skip work needlessly).
    let tmp = match side.file_name().and_then(|name| name.to_str()) {
        Some(name) => {
            side.with_file_name(format!("{}.subcrate-link-tmp-{}", name, std::process::id()))
        }
        None => return,
    };
    let _ = std::fs::remove_file(&tmp);
    if std::fs::hard_link(main, &tmp).is_ok() && std::fs::rename(&tmp, side).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Whether two paths already refer to one file (a hard link made on an earlier
/// build), so there is nothing to reclaim. A false negative only repeats the
/// content check and relink; a false positive only skips a dedup — neither can
/// affect correctness, since same-named artifacts always hold the same bytes.
#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// Windows exposes no file identity on stable without opening a handle, so this
/// compares size and last-write time: a hard link shares the underlying file's
/// metadata, so a match means `a` is already linked to `b`. Freshly built copies
/// differ in write time, so the first link is still made; afterwards the linked
/// file reports `b`'s time and is skipped.
#[cfg(windows)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => {
            x.file_size() == y.file_size() && x.last_write_time() == y.last_write_time()
        }
        _ => false,
    }
}

#[cfg(not(any(unix, windows)))]
fn same_file(_a: &Path, _b: &Path) -> bool {
    false
}

/// Whether two files hold the same bytes. A size comparison comes first; the
/// names are content-addressed, so a match here is all but certain and this only
/// confirms it before anything is replaced.
fn same_contents(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) if ma.len() == mb.len() => {
            matches!((std::fs::read(a), std::fs::read(b)), (Ok(da), Ok(db)) if da == db)
        }
        _ => false,
    }
}

/// A name for the compiler `rustc`, fit for a directory name: its release and
/// commit, from `rustc -vV` (`1.56.0-09c42c458`). `None` if that cannot be had.
fn rustc_key(rustc: &Path) -> Option<String> {
    let output = Command::new(rustc).arg("-vV").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .map(str::trim)
    };
    let release = field("release:")?;
    let commit = field("commit-hash:").unwrap_or("");
    let key: String = format!("{}-{}", release, commit.chars().take(9).collect::<String>())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(key)
}

/// A discovered `#[subcrate]` module.
struct Subcrate {
    /// The module's path from its crate root, ending in its own identifier
    /// (e.g. `["nested", "inner"]`).
    module_path: Vec<String>,
    /// The crate-root file the module was discovered from, which scopes
    /// dependency resolution (see [`resolve_dependency`]).
    root: PathBuf,
    /// The file the `#[subcrate]` module is written in (the crate root, or a file
    /// reached by `mod foo;`). Its tokens' spans point into this file, so a `rustc`
    /// diagnostic on the generated source can be reported against it.
    source_file: PathBuf,
    /// Whether the body pulled in other files via `mod foo;` (so its tokens span
    /// several files). When it did, spans cannot be mapped to one file, and
    /// diagnostics keep the generated `<anon>` name.
    inlined_files: bool,
    /// The crates this subcrate links against, as written in its arguments.
    deps: Vec<Dependency>,
    /// The generated, content-addressed crate name.
    internal: String,
    /// Whether to compile as a `proc-macro` crate.
    proc_macro: bool,
    /// A `#[subcrate(edition = "…")]` override, if any.
    edition: Option<String>,
    /// The subcrate's crate-root inner attributes (`#![no_std]`, `#![allow(…)]`).
    inner_attrs: Vec<Attribute>,
    /// The module body (its items), used as the crate root source.
    items: Vec<Item>,
    /// UI-test expectations from `#[subcrate(should_error/warn/spanned …)]`. When
    /// any is set the subcrate is a UI test (`ui.is_ui_test()`; see
    /// [`run_ui_check`]).
    ui: UiExpectations,
    /// Whether the module also carries a `#[rustversion::…]` gate (see
    /// [`is_rustversion_gate`]). When it does, a failure to compile is treated as
    /// "not available on this toolchain" and the subcrate is skipped rather than
    /// failing the build — matching rustversion's own removal of the module.
    gated: bool,
}

impl Subcrate {
    /// The module path as written in source, e.g. `nested::inner`.
    fn display_name(&self) -> String {
        self.module_path.join("::")
    }

    /// File name `rustc` gives this subcrate's artifact in the output directory:
    /// an `rlib` for a library, or a dynamic library (named for the *host*, where
    /// proc-macros run) for a proc-macro.
    fn artifact_file(&self) -> String {
        if !self.proc_macro {
            format!("lib{}.rlib", self.internal)
        } else if cfg!(windows) {
            format!("{}.dll", self.internal)
        } else if cfg!(target_os = "macos") {
            format!("lib{}.dylib", self.internal)
        } else {
            format!("lib{}.so", self.internal)
        }
    }
}

/// Result of scanning the package for subcrates.
#[derive(Default)]
struct Discovered {
    subcrates: Vec<Subcrate>,
    files: Vec<PathBuf>,
}

/// Scans the package, starting from its crate roots, and collects every
/// `#[subcrate]` module.
fn discover(manifest_dir: &Path) -> Discovered {
    let mut walker = Walker::default();
    for (file, mod_dir) in crate_roots(manifest_dir) {
        // Canonical so a subcrate's crate root compares equal to the owner's library
        // and to its siblings however each was spelled (see [`canonical_path`]).
        walker.root = canonical_path(&file);
        walker.walk_file(&file, &mod_dir);
    }
    walker.out
}

/// A path in canonical form for comparison — symlinks resolved, `.`/`..` collapsed —
/// falling back to the path as given when it cannot be resolved (e.g. it does not
/// exist). Crate roots reach discovery spelled several ways: the default location, a
/// `[lib]`/`[[test]]`/… `path`, or through a symlinked directory. Comparing them
/// (library vs. subcrate root, or sibling roots) by their canonical form keeps those
/// spellings of one file equal, so a library subcrate is not mistaken for a test's,
/// nor two siblings for strangers.
fn canonical_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Enumerates the crate-root files and the directory their top-level modules
/// resolve against.
///
/// These are the targets Cargo builds, found the way Cargo finds them: the library
/// and, unless the manifest turns that off, the files it discovers by their place
/// (`src/main.rs` and `src/bin/`, `tests/`, `benches/`, `examples/` — see
/// [`AutoDiscovery`]), plus every target the manifest declares, wherever its source is.
fn crate_roots(manifest_dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let manifest = std::fs::read_to_string(manifest_dir.join("Cargo.toml")).unwrap_or_default();
    let auto = auto_discovery(&manifest);
    let mut roots = Vec::new();
    let src = manifest_dir.join("src");

    // `autolib` (Cargo 1.83+) is not read: a library nobody builds costs nothing
    // here, as `src/lib.rs` is rarely not the library, and in a package that has a
    // `[lib]` of its own its path is added below.
    let lib = src.join("lib.rs");
    if lib.is_file() {
        roots.push((lib, src.clone()));
    }
    if auto.bins {
        let main = src.join("main.rs");
        if main.is_file() {
            roots.push((main, src.clone()));
        }
        // Binaries: `src/bin/*.rs` and `src/bin/<name>/main.rs`.
        collect_root_dir(&src.join("bin"), &mut roots);
    }
    if auto.tests {
        // Integration tests: `tests/*.rs` and `tests/<name>/main.rs`.
        collect_root_dir(&manifest_dir.join("tests"), &mut roots);
    }
    if auto.benches {
        // Benchmarks: `benches/*.rs` and `benches/<name>/main.rs`.
        collect_root_dir(&manifest_dir.join("benches"), &mut roots);
    }
    if auto.examples {
        // Examples: `examples/*.rs` and `examples/<name>/main.rs`.
        collect_root_dir(&manifest_dir.join("examples"), &mut roots);
    }

    // Targets the manifest declares: a `[lib] path = "…"`, or a
    // `[[bin]]`/`[[test]]`/`[[bench]]`/`[[example]]`. Cargo lets any target live
    // anywhere, so the library is not always `src/lib.rs`. A `path` is relative to the
    // manifest directory and may point outside it (`../shared`), which a workspace can
    // share between members. A target without a `path` is where Cargo looks for it
    // by its name. These are added on top of the defaults above, which are all there
    // is when discovery is on, and `Walker` dedups by canonical path, so an overlap
    // compiles once.
    let package = package_name(&manifest);
    for target in declared_targets(&manifest) {
        let candidates = match &target.path {
            Some(path) => vec![manifest_dir.join(path)],
            None => default_target_files(
                manifest_dir,
                &target.kind,
                target.name.as_deref(),
                package.as_deref(),
            ),
        };
        for file in candidates.into_iter().filter(|file| file.is_file()) {
            let dir = file
                .parent()
                .map_or_else(|| manifest_dir.to_path_buf(), Path::to_path_buf);
            roots.push((file, dir));
        }
    }

    roots
}

/// Which kinds of targets Cargo finds by looking at the file system: `src/main.rs`
/// and `src/bin/` (`autobins`), `tests/` (`autotests`), `benches/` (`autobenches`),
/// `examples/` (`autoexamples`). Each can be turned off in `[package]`, leaving the
/// targets the manifest declares as the only ones of their kind — the way for a
/// package to have files in those directories that are not its own, as when the
/// directory holds another package (a workspace whose root package has its tests in
/// a package of their own in `tests/`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AutoDiscovery {
    bins: bool,
    tests: bool,
    benches: bool,
    examples: bool,
}

impl Default for AutoDiscovery {
    fn default() -> Self {
        AutoDiscovery {
            bins: true,
            tests: true,
            benches: true,
            examples: true,
        }
    }
}

impl AutoDiscovery {
    /// The directories of the package that hold crate roots, and so are watched for
    /// files coming and going: `src`, and the others unless discovery is off for them.
    fn root_dirs(self) -> Vec<&'static str> {
        let mut dirs = vec!["src"];
        for (dir, on) in [
            ("tests", self.tests),
            ("benches", self.benches),
            ("examples", self.examples),
        ] {
            if on {
                dirs.push(dir);
            }
        }
        dirs
    }
}

/// What a manifest's `[package]` says about [`AutoDiscovery`]: on unless
/// `autobins`/`autotests`/`autobenches`/`autoexamples` is `false`.
fn auto_discovery(manifest: &str) -> AutoDiscovery {
    let mut auto = AutoDiscovery::default();
    for (key, value) in package_entries(manifest) {
        let slot = match key.as_str() {
            "autobins" => &mut auto.bins,
            "autotests" => &mut auto.tests,
            "autobenches" => &mut auto.benches,
            "autoexamples" => &mut auto.examples,
            _ => continue,
        };
        match value.trim() {
            "false" => *slot = false,
            "true" => *slot = true,
            _ => {}
        }
    }
    auto
}

/// The `[package]` table of a manifest as `(key, value)` pairs, the value as written
/// (without a comment).
fn package_entries(manifest: &str) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    let mut in_package = false;
    for raw in manifest.lines() {
        let line = strip_toml_comment(raw).trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_package = line[1..line.len() - 1].trim() == "package";
        } else if in_package {
            if let Some((key, value)) = split_key_value(line) {
                entries.push((key.trim().to_string(), value.trim().to_string()));
            }
        }
    }
    entries
}

/// The `[package]` name of a manifest.
fn package_name(manifest: &str) -> Option<String> {
    package_entries(manifest)
        .into_iter()
        .find(|(key, _)| key == "name")
        .and_then(|(_, value)| toml_string_value(&value))
}

/// A target a `Cargo.toml` declares: its `[lib]` table, or an entry of the
/// `[[bin]]`, `[[test]]`, `[[bench]]` or `[[example]]` arrays of tables.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeclaredTarget {
    /// `lib`, `bin`, `test`, `bench` or `example`.
    kind: String,
    name: Option<String>,
    /// Where the source is, relative to the manifest directory, if the manifest says.
    path: Option<String>,
}

/// Every target a `Cargo.toml` declares: the `[lib]` table and the `[[bin]]`,
/// `[[test]]`, `[[bench]]` and `[[example]]` arrays of tables, with the `name` and
/// `path` they give. These let a target's source live anywhere, so the library need
/// not be `src/lib.rs`.
fn declared_targets(manifest: &str) -> Vec<DeclaredTarget> {
    let mut targets = Vec::new();
    let mut current: Option<DeclaredTarget> = None;
    for raw in manifest.lines() {
        let line = strip_toml_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            targets.extend(current.take());
            // `[lib]`, or an array-of-tables header like `[[bin]]` (inner `[bin]`).
            let inner = line[1..line.len() - 1]
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim();
            current = match inner {
                "lib" | "bin" | "test" | "bench" | "example" => Some(DeclaredTarget {
                    kind: inner.to_string(),
                    name: None,
                    path: None,
                }),
                _ => None,
            };
            continue;
        }
        let target = match &mut current {
            Some(target) => target,
            None => continue,
        };
        // `name = "…"` and `path = "…"`, each as its own key (not a prefix like
        // `pathsep`).
        if let Some((key, value)) = split_key_value(line) {
            let slot = match key.trim() {
                "name" => &mut target.name,
                "path" => &mut target.path,
                _ => continue,
            };
            if let Some(value) = toml_string_value(value.trim()) {
                *slot = Some(value);
            }
        }
    }
    targets.extend(current);
    targets
}

/// The `path` of the `[lib]` table of a `Cargo.toml`, if it has one.
fn lib_source_path(manifest: &str) -> Option<String> {
    declared_targets(manifest)
        .into_iter()
        .find(|target| target.kind == "lib")
        .and_then(|target| target.path)
}

/// Where Cargo looks for the source of a declared target that gives no `path`, by its
/// `name`: `tests/<name>.rs` or `tests/<name>/main.rs` for a test (likewise for
/// benchmarks and examples), `src/bin/<name>.rs` or `src/bin/<name>/main.rs` for a
/// binary, which is `src/main.rs` if it is named like the package. The library's is
/// `src/lib.rs`, already a root.
fn default_target_files(
    manifest_dir: &Path,
    kind: &str,
    name: Option<&str>,
    package: Option<&str>,
) -> Vec<PathBuf> {
    let dir = match kind {
        "bin" => manifest_dir.join("src").join("bin"),
        "test" => manifest_dir.join("tests"),
        "bench" => manifest_dir.join("benches"),
        "example" => manifest_dir.join("examples"),
        _ => return Vec::new(),
    };
    let name = match name {
        Some(name) => name,
        None => return Vec::new(),
    };
    let mut files = Vec::new();
    if kind == "bin" && Some(name) == package {
        files.push(manifest_dir.join("src").join("main.rs"));
    }
    files.push(dir.join(format!("{}.rs", name)));
    files.push(dir.join(name).join("main.rs"));
    files
}

/// Collects root files from a directory that may contain both `*.rs` files and
/// `<name>/main.rs` subdirectories (the shape used by `src/bin` and `tests`).
fn collect_root_dir(dir: &Path, roots: &mut Vec<(PathBuf, PathBuf)>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            roots.push((path, dir.to_path_buf()));
        } else if path.is_dir() {
            let main = path.join("main.rs");
            if main.is_file() {
                roots.push((main, path));
            }
        }
    }
}

#[derive(Default)]
struct Walker {
    out: Discovered,
    visited: HashSet<PathBuf>,
    seen_internal: HashSet<String>,
    /// The crate-root file currently being walked.
    root: PathBuf,
    /// The file currently being walked (the crate root, or one reached by
    /// `mod foo;`). A subcrate's tokens' spans point into this file.
    current_file: PathBuf,
    /// Module path (from `root`) of the items currently being walked.
    mod_path: Vec<String>,
}

impl Walker {
    fn walk_file(&mut self, path: &Path, mod_dir: &Path) {
        let key = canonical_path(path);
        if !self.visited.insert(key) {
            return;
        }

        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(_) => return,
        };
        // A file that does not parse will be reported by the real compiler;
        // don't mask that error from the build script.
        let file = match syn::parse_file(&text) {
            Ok(file) => file,
            Err(_) => {
                warn(&format!(
                    "subcrate: could not parse `{}` while scanning for #[subcrate] modules",
                    path.display()
                ));
                return;
            }
        };

        self.out.files.push(path.to_path_buf());
        // Track the file being walked (restored on return), so a `mod foo;` that
        // recurses into another file does not leave its name set for the items
        // after it in this one.
        let prev_file = std::mem::replace(&mut self.current_file, path.to_path_buf());
        self.walk_items(&file.items, mod_dir);
        self.current_file = prev_file;
    }

    fn walk_items(&mut self, items: &[Item], mod_dir: &Path) {
        for item in items {
            let module = match item {
                Item::Mod(module) => module,
                _ => continue,
            };

            if let Some(attr) = module.attrs.iter().find(|a| is_subcrate_attr(a)) {
                self.record_subcrate(module, attr, mod_dir);
                // A subcrate is its own crate; do not descend into it.
                continue;
            }

            let path_attr = module.attrs.iter().find_map(path_attr_value);
            // The module path follows the `mod` nesting, not `#[path]`.
            self.mod_path.push(module.ident.to_string());
            match &module.content {
                Some((_, inner)) => {
                    // Inline module: children live under `<mod_dir>/<ident>`
                    // unless `#[path]` redirects them.
                    let child_dir = match path_attr {
                        Some(p) => mod_dir.join(p),
                        None => mod_dir.join(module.ident.to_string()),
                    };
                    self.walk_items(inner, &child_dir);
                }
                None => {
                    // External module `mod foo;`: resolve to a file.
                    if let Some((file, child_dir)) = resolve_module_file(
                        mod_dir,
                        &module.ident.to_string(),
                        path_attr.as_deref(),
                    ) {
                        self.walk_file(&file, &child_dir);
                    }
                }
            }
            self.mod_path.pop();
        }
    }

    fn record_subcrate(&mut self, module: &syn::ItemMod, attr: &Attribute, mod_dir: &Path) {
        let args = match parse_attr_args(attr) {
            Ok(args) => args,
            Err(err) => {
                warn(&format!(
                    "subcrate: ignoring `#[subcrate]` on `{}`: {}",
                    module.ident, err
                ));
                return;
            }
        };

        let items = match &module.content {
            Some((_, items)) => items,
            None => {
                // The macro will emit a precise compile error; nothing to compile.
                warn(&format!(
                    "subcrate: `#[subcrate]` on `{}` must be an inline module with a body",
                    module.ident
                ));
                return;
            }
        };

        let ident = module.ident.to_string();
        let mut module_path = self.mod_path.clone();
        module_path.push(ident.clone());
        let inner = subcrate_core::inner_attrs(&module.attrs);
        // The name is computed from the *original* body (as written, with any
        // `mod foo;` left as a declaration) so it matches what the macro — which
        // never reads those files — computes.
        let internal = internal_crate_name(&ident, &args, &inner, items);

        // The body that is actually compiled, however, inlines external
        // `mod foo;` files (the subcrate is fed to `rustc` on stdin, which has no
        // directory to resolve them against). Children resolve under the
        // subcrate module's own directory.
        let body_dir = match module.attrs.iter().find_map(path_attr_value) {
            Some(p) => mod_dir.join(p),
            None => mod_dir.join(&ident),
        };
        let mut extra_files = Vec::new();
        let compiled_items = inline_external_mods(items, &body_dir, &mut extra_files);
        let inlined_files = !extra_files.is_empty();
        self.out.files.extend(extra_files);

        let gated = module.attrs.iter().any(is_rustversion_gate);
        if self.seen_internal.insert(internal.clone()) {
            self.out.subcrates.push(Subcrate {
                module_path,
                root: self.root.clone(),
                source_file: self.current_file.clone(),
                inlined_files,
                deps: args.deps,
                internal,
                proc_macro: args.proc_macro,
                edition: args.edition,
                inner_attrs: inner,
                items: compiled_items,
                ui: args.ui,
                gated,
            });
        }
    }
}

/// A dependency linked into a subcrate being compiled.
struct ExternDep {
    /// The dependency's crate name (what `--extern` names it): the internal name
    /// of a sibling subcrate, or the real name of an owner dependency.
    internal: String,
    /// The name the dependent's source sees it under.
    alias: String,
    /// The dependency's compiled artifacts, each passed as `--extern <internal>=<path>`:
    /// one for a subcrate, an rlib and its rmeta for an owner dependency.
    paths: Vec<PathBuf>,
    /// Whether this is — or, for a subcrate, is built on — an owner dependency or
    /// the owner crate. Cargo builds those, so their files' names do not say
    /// whether they changed, and the directories holding the crates they link
    /// against have to be searched.
    owner_linked: bool,
}

/// The owning package's library, as seen when resolving dependencies.
struct Owner {
    /// The library's crate name: the package name with `-` as `_`.
    name: String,
    /// The library's root (`[lib] path`, or `src/lib.rs`), if the package has one.
    lib: Option<PathBuf>,
}

/// What a subcrate's dependency refers to.
#[derive(Debug, PartialEq, Eq)]
enum LinkTarget {
    /// A sibling `#[subcrate]` module (an index into the discovered subcrates).
    Subcrate(usize),
    /// A crate the owning package depends on, `::name`.
    External(String),
    /// The owning package's own library, `::<package name>`.
    Owner,
}

/// A resolved dependency: `target` is linked under the name `alias`.
#[derive(Debug)]
struct Link {
    alias: String,
    target: LinkTarget,
}

impl Link {
    /// The sibling subcrate this links to, if it is one.
    fn subcrate(&self) -> Option<usize> {
        match self.target {
            LinkTarget::Subcrate(ix) => Some(ix),
            _ => None,
        }
    }
}

/// Resolves every subcrate's declared dependencies — to discovered subcrates, to
/// crates the package depends on (`::name`), or to the package's own library —
/// returning, for each subcrate (by index), its links sorted by alias.
fn resolve_links(subcrates: &[Subcrate], owner: &Owner) -> Result<Vec<Vec<Link>>, Error> {
    let mut all = Vec::with_capacity(subcrates.len());
    for (ix, sub) in subcrates.iter().enumerate() {
        let mut links: Vec<Link> = Vec::new();
        for dep in &sub.deps {
            if links.iter().any(|link| link.alias == dep.alias) {
                return Err(Error::new(format!(
                    "subcrate `{}` has more than one dependency named `{}`; \
                     rename one with `alias = path::to::subcrate`",
                    sub.display_name(),
                    dep.alias
                )));
            }
            let target = if dep.external {
                resolve_external(sub, dep, owner)?
            } else {
                LinkTarget::Subcrate(resolve_dependency(subcrates, ix, dep)?)
            };
            links.push(Link {
                alias: dep.alias.clone(),
                target,
            });
        }
        links.sort_by(|a, b| a.alias.cmp(&b.alias));
        all.push(links);
    }
    Ok(all)
}

/// Resolves a `::name` dependency of `sub`. It is a crate the package depends on,
/// or, when `name` is the package's own library, the owner crate. The library's
/// own subcrates cannot use the owner crate, since the library would depend on
/// itself.
fn resolve_external(sub: &Subcrate, dep: &Dependency, owner: &Owner) -> Result<LinkTarget, Error> {
    let name = match dep.path.as_slice() {
        [name] => name,
        _ => {
            return Err(Error::new(format!(
                "subcrate `{}`: `{}` is a path into a crate, but an external dependency names \
                 the crate itself, like `::{}`",
                sub.display_name(),
                dep.path_display(),
                dep.path[0]
            )))
        }
    };
    if *name != owner.name {
        return Ok(LinkTarget::External(name.clone()));
    }
    match &owner.lib {
        None => Err(Error::new(format!(
            "subcrate `{}` depends on `::{}`, the owner crate, but this package has no library \
             (`src/lib.rs`, or a `[lib] path`) for it to depend on",
            sub.display_name(),
            name
        ))),
        Some(lib) if sub.root == *lib => Err(Error::new(format!(
            "subcrate `{}` is part of the library `{}` and cannot depend on `::{}`, which is that \
             library itself: that is a dependency cycle. Only subcrates outside the library (in \
             `tests/`, `src/main.rs`, `src/bin/`, `examples/` or `benches/`) can use the owner \
             crate",
            sub.display_name(),
            name,
            name
        ))),
        Some(_) => Ok(LinkTarget::Owner),
    }
}

/// Finds the subcrate that `dep`, declared by `subcrates[from]`, refers to.
///
/// A dependency names a module path from a crate root. A subcrate whose full
/// module path equals it wins; failing that, one whose path merely *ends* with it
/// matches, so a lone ident `a` finds a subcrate named `a` wherever it is nested.
/// If that is still ambiguous, a subcrate under the same crate root as the
/// dependent wins — otherwise two `tests/*.rs` files could never each depend on
/// their own `helper`. Anything left over is an error.
fn resolve_dependency(
    subcrates: &[Subcrate],
    from: usize,
    dep: &Dependency,
) -> Result<usize, Error> {
    let path = &dep.path;
    let suffix: Vec<usize> = (0..subcrates.len())
        .filter(|&i| subcrates[i].module_path.ends_with(path))
        .collect();
    let exact: Vec<usize> = suffix
        .iter()
        .copied()
        .filter(|&i| subcrates[i].module_path == *path)
        .collect();
    let mut candidates = if exact.is_empty() { suffix } else { exact };

    if candidates.len() > 1 {
        let same_root: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&i| subcrates[i].root == subcrates[from].root)
            .collect();
        if same_root.len() == 1 {
            candidates = same_root;
        }
    }

    match candidates.as_slice() {
        [only] => Ok(*only),
        [] => Err(Error::new(format!(
            "subcrate `{}` depends on `{}`, but no `#[subcrate]` module with that path was found \
             in this package. (To depend on a crate the package depends on, write `::{}`.)",
            subcrates[from].display_name(),
            dep.path_display(),
            dep.path_display()
        ))),
        many => {
            // Say which file each candidate is in only when that tells them apart.
            let one_root = many
                .iter()
                .all(|&i| subcrates[i].root == subcrates[many[0]].root);
            let listed: Vec<String> = many
                .iter()
                .map(|&i| {
                    if one_root {
                        format!("`{}`", subcrates[i].display_name())
                    } else {
                        format!(
                            "`{}` (in `{}`)",
                            subcrates[i].display_name(),
                            subcrates[i].root.display()
                        )
                    }
                })
                .collect();
            Err(Error::new(format!(
                "subcrate `{}` depends on `{}`, which is ambiguous; it matches {}. \
                 Use a longer module path to pick one, or rename a module",
                subcrates[from].display_name(),
                dep.path_display(),
                listed.join(", ")
            )))
        }
    }
}

/// The sibling dependencies of `subcrates[ix]` the macro can name in the
/// `cfg(doc)` stand-in: those under the same crate root, where `crate::<module
/// path>` is valid. (A dependency from another crate root has no such path; an
/// external crate needs none, as the macro imports it by name.)
fn doc_deps(subcrates: &[Subcrate], ix: usize, links: &[Link]) -> Vec<ResolvedDep> {
    links
        .iter()
        .filter_map(|link| Some((link, link.subcrate()?)))
        .filter(|(_, target)| subcrates[*target].root == subcrates[ix].root)
        .map(|(link, target)| (link.alias.clone(), subcrates[target].module_path.clone()))
        .collect()
}

/// For each subcrate (by index), the subcrates that must be compiled before it:
/// its sibling dependencies and, if it uses the owner crate, every subcrate of the
/// owner's library — the library is built from them, so they have to exist before
/// anything can link against it.
fn compile_deps(subcrates: &[Subcrate], links: &[Vec<Link>], owner: &Owner) -> Vec<Vec<usize>> {
    let lib_subcrates: Vec<usize> = (0..subcrates.len())
        .filter(|&ix| owner.lib.as_deref() == Some(subcrates[ix].root.as_path()))
        .collect();
    links
        .iter()
        .map(|links| {
            let mut needs = Vec::new();
            for link in links {
                match link.target {
                    LinkTarget::Subcrate(target) => needs.push(target),
                    LinkTarget::Owner => needs.extend(lib_subcrates.iter().copied()),
                    LinkTarget::External(_) => {}
                }
            }
            needs.sort_unstable();
            needs.dedup();
            needs
        })
        .collect()
}

/// Which subcrates a nested build (of the library only) needs: those in the
/// library and, transitively, everything they depend on.
fn needed_by_lib(subcrates: &[Subcrate], needs: &[Vec<usize>], owner: &Owner) -> Vec<bool> {
    let mut wanted = vec![false; subcrates.len()];
    let mut stack: Vec<usize> = (0..subcrates.len())
        .filter(|&ix| owner.lib.as_deref() == Some(subcrates[ix].root.as_path()))
        .collect();
    while let Some(ix) = stack.pop() {
        if !wanted[ix] {
            wanted[ix] = true;
            stack.extend(needs[ix].iter().copied());
        }
    }
    wanted
}

/// Which subcrates (by index) link against an owner dependency or the owner crate,
/// directly or through the sibling subcrates they depend on. `order` lists the
/// subcrates dependencies first.
fn owner_linked(links: &[Vec<Link>], order: &[usize]) -> Vec<bool> {
    let mut linked = vec![false; links.len()];
    for &ix in order {
        linked[ix] = links[ix].iter().any(|link| match link.target {
            LinkTarget::Subcrate(target) => linked[target],
            LinkTarget::External(_) | LinkTarget::Owner => true,
        });
    }
    linked
}

/// Orders the subcrates (by index) so that each one comes after everything it
/// needs (see [`compile_deps`]), using [`safegraph`]'s topological sort. A
/// dependency cycle is an error naming the cycle.
fn compile_order(subcrates: &[Subcrate], needs: &[Vec<usize>]) -> Result<Vec<usize>, Error> {
    // One node per subcrate and one edge per dependency, pointing from the
    // dependency to its dependent ("compile this first"). A dependency declared
    // under two aliases is still a single edge.
    let mut graph = BTreeGraph::<usize, usize>::default();
    let graph_error = |what: &str, err: &dyn std::fmt::Debug| {
        Error::new(format!(
            "internal error building the subcrate dependency graph ({}): {:?}",
            what, err
        ))
    };
    for ix in 0..subcrates.len() {
        graph.insert_node(ix).map_err(|e| graph_error("node", &e))?;
    }
    let edges: BTreeSet<(usize, usize)> = needs
        .iter()
        .enumerate()
        .flat_map(|(ix, needs)| needs.iter().map(move |&dependency| (dependency, ix)))
        .collect();
    for (edge, (dependency, dependent)) in edges.into_iter().enumerate() {
        graph
            .insert_edge(edge, [dependency, dependent])
            .map_err(|e| graph_error("edge", &e))?;
    }

    toposort(&graph).map_err(|cycle| {
        let path: Vec<String> = find_cycle(needs, cycle.node)
            .into_iter()
            .map(|ix| subcrates[ix].display_name())
            .collect();
        Error::new(format!("subcrate dependency cycle: {}", path.join(" -> ")))
    })
}

/// Finds a path `start -> … -> start` along "needs" edges, for the error message.
/// `start` is known to lie on a cycle, so one exists.
fn find_cycle(needs: &[Vec<usize>], start: usize) -> Vec<usize> {
    fn visit(
        needs: &[Vec<usize>],
        node: usize,
        start: usize,
        seen: &mut Vec<bool>,
        path: &mut Vec<usize>,
    ) -> bool {
        path.push(node);
        for &target in &needs[node] {
            if target == start {
                path.push(start);
                return true;
            }
            if !seen[target] {
                seen[target] = true;
                if visit(needs, target, start, seen, path) {
                    return true;
                }
            }
        }
        path.pop();
        false
    }

    let mut path = Vec::new();
    let mut seen = vec![false; needs.len()];
    seen[start] = true;
    visit(needs, start, start, &mut seen, &mut path);
    path
}

/// Recursively inlines external `mod foo;` declarations (and the modules they
/// reach) into the token tree, so a subcrate body spread across multiple files
/// can be compiled as a single stdin crate root. Every file read is pushed to
/// `files` for change tracking. Modules that cannot be resolved or parsed are
/// left untouched, so `rustc` reports a precise error.
fn inline_external_mods(items: &[Item], mod_dir: &Path, files: &mut Vec<PathBuf>) -> Vec<Item> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let module = match item {
            Item::Mod(module) => module,
            other => {
                out.push(other.clone());
                continue;
            }
        };
        let path_attr = module.attrs.iter().find_map(path_attr_value);
        match &module.content {
            Some((_, inner)) => {
                // Inline module: recurse into it under the nested directory.
                let child_dir = match &path_attr {
                    Some(p) => mod_dir.join(p),
                    None => mod_dir.join(module.ident.to_string()),
                };
                let inlined = inline_external_mods(inner, &child_dir, files);
                out.push(rebuild_inline_mod(module, inlined, &[]));
            }
            None => {
                // External module: resolve the file, parse it, and splice it in.
                let resolved =
                    resolve_module_file(mod_dir, &module.ident.to_string(), path_attr.as_deref());
                match resolved.and_then(|(file, child_dir)| {
                    read_rust_file(&file).map(|parsed| (file, child_dir, parsed))
                }) {
                    Some((file, child_dir, parsed)) => {
                        files.push(file);
                        let inlined = inline_external_mods(&parsed.items, &child_dir, files);
                        out.push(rebuild_inline_mod(module, inlined, &parsed.attrs));
                    }
                    None => out.push(item.clone()),
                }
            }
        }
    }
    out
}

/// Reads and parses a Rust source file, warning (and returning `None`) on error.
fn read_rust_file(path: &Path) -> Option<syn::File> {
    let text = std::fs::read_to_string(path).ok()?;
    match syn::parse_file(&text) {
        Ok(file) => Some(file),
        Err(_) => {
            warn(&format!("subcrate: could not parse `{}`", path.display()));
            None
        }
    }
}

/// Rebuilds a module as an inline module containing `items`. Keeps its outer
/// attributes (dropping the now-resolved `#[path]`) and its own inner
/// attributes, and adds `file_inner` — the crate-level `#![…]` attributes of an
/// included file — as inner attributes of the module.
fn rebuild_inline_mod(module: &syn::ItemMod, items: Vec<Item>, file_inner: &[Attribute]) -> Item {
    let outer = module
        .attrs
        .iter()
        .filter(|a| matches!(a.style, syn::AttrStyle::Outer) && !a.path().is_ident("path"));
    let own_inner = module
        .attrs
        .iter()
        .filter(|a| matches!(a.style, syn::AttrStyle::Inner(_)));
    let vis = &module.vis;
    let ident = &module.ident;
    let tokens = quote! {
        #(#outer)*
        #vis mod #ident {
            #(#own_inner)*
            #(#file_inner)*
            #(#items)*
        }
    };
    syn::parse2(tokens).expect("a reconstructed inline module should re-parse")
}

/// Resolves `mod <name>;` to a source file and the directory its own children
/// resolve against, following the standard Rust rules and `#[path]`.
fn resolve_module_file(
    mod_dir: &Path,
    name: &str,
    path_attr: Option<&str>,
) -> Option<(PathBuf, PathBuf)> {
    if let Some(p) = path_attr {
        let file = mod_dir.join(p);
        let child_dir = file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| mod_dir.to_path_buf());
        return if file.is_file() {
            Some((file, child_dir))
        } else {
            None
        };
    }

    let child_dir = mod_dir.join(name);
    let flat = mod_dir.join(format!("{}.rs", name));
    if flat.is_file() {
        return Some((flat, child_dir));
    }
    let nested = child_dir.join("mod.rs");
    if nested.is_file() {
        return Some((nested, child_dir));
    }
    None
}

/// Extracts the string value of a `#[path = "…"]` attribute.
/// Whether `attr` is a `#[rustversion::<selector>]` gate — one of rustversion's
/// *conditional-compilation* selectors, which keep or remove the item they are on.
///
/// Matched by path (`rustversion :: <selector>`), so detection needs no toolchain
/// knowledge: when a `#[subcrate]` module carries one, the build helper treats a
/// failure to compile as "not available on this toolchain" and skips it (see
/// [`Config::compile_all`]). `rustversion::attr(…)` is *not* a gate — it adds an
/// attribute rather than removing the item — so it is excluded.
fn is_rustversion_gate(attr: &Attribute) -> bool {
    let path = attr.path();
    let first = path.segments.first();
    let last = path.segments.last();
    match (first, last) {
        (Some(first), Some(last)) if first.ident == "rustversion" => matches!(
            last.ident.to_string().as_str(),
            "stable" | "beta" | "nightly" | "since" | "before" | "not" | "any" | "all"
        ),
        _ => false,
    }
}

fn path_attr_value(attr: &Attribute) -> Option<String> {
    if !attr.path().is_ident("path") {
        return None;
    }
    if let Meta::NameValue(nv) = &attr.meta {
        if let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) = &nv.value
        {
            return Some(s.value());
        }
    }
    None
}

/// Renders a module body into crate-root source text: crate-level inner
/// attributes first (they must lead the crate root), then the import of each
/// linked dependency under its alias, then the items.
///
/// The import is `pub extern crate <internal name> as <alias>;`. This binds
/// `alias` into the subcrate's *extern prelude* (on every edition), so the body can
/// reach the dependency as `::<alias>` and, because it is `pub`, re-export the crate
/// itself with `pub use <alias>;` (a private `extern crate` cannot be re-exported —
/// E0365) as well as its items with `pub use <alias>::…;`. It is an injected
/// render-time detail and is not part of the subcrate's content-addressed name (the
/// dependency *list* in the arguments is). It sits at the crate root, so nested
/// modules reach the alias as `crate::<alias>` or `super::<alias>`.
/// Renders a subcrate's source, and a map from each generated line (1-based) to
/// the original source line it came from — used to rewrite `rustc`'s `<anon>:L:C`
/// diagnostics to the user's real file and line. The map is empty (so diagnostics
/// keep the `<anon>` name) when `mappable` is false — the body was pulled from
/// several files via `mod foo;`, whose spans would point at the wrong file — or
/// when the pretty form fails to re-parse and the compact fallback is used.
fn render_source(
    inner_attrs: &[Attribute],
    deps: &[ExternDep],
    items: &[Item],
    mappable: bool,
    proc_macro: bool,
) -> (String, Vec<Option<usize>>) {
    let attrs = quote! { #(#inner_attrs)* }.to_string();
    let mut imports = String::new();
    for dep in deps {
        // `pub` (for a library) so the body can re-export the dependency
        // (`pub use <alias>;`): a plain `extern crate` is private, and re-exporting
        // a private one is an error (E0365). It only makes the alias re-exportable;
        // the body still has to opt in. A proc-macro crate may export nothing but
        // its macros, so there the `extern crate` stays private (re-export would be
        // both impossible and pointless). `unused_extern_crates` is allowed: a
        // dependency need not be used.
        let vis = if proc_macro { "" } else { "pub " };
        imports.push_str(&format!(
            "#[allow(unused_extern_crates)] {}extern crate {} as {};\n",
            vis, dep.internal, dep.alias
        ));
    }
    let items = quote! { #(#items)* };
    // Pretty-print the body (multi-line) so a `rustc` diagnostic's line and column
    // point at the right construct, not a column deep into one long line.
    // `pretty_tokens` only adds whitespace, but fall back to the compact form if
    // the result somehow fails to re-parse, so the compiled source is always valid.
    let (body, body_map) = pretty_tokens(&items);
    let pretty = format!("{}\n{}{}", attrs, imports, body);
    if syn::parse_file(&pretty).is_err() {
        return (format!("{}\n{}{}", attrs, imports, items), Vec::new());
    }
    if !mappable {
        return (pretty, Vec::new());
    }
    // The body follows the preamble — the crate-root attributes and the injected
    // `extern crate` imports — which have no user-source line. Shift the body's
    // per-line map past those.
    let preamble_lines = attrs.matches('\n').count() + 1 + imports.matches('\n').count();
    let mut map = vec![None; preamble_lines + 1];
    map.extend(body_map);
    (pretty, map)
}

/// Renders a token stream as multi-line source (see [`render_source`]) and, for
/// each output line, the original source line of the tokens on it (0-based,
/// `None` where no spanned token landed). Only inserts whitespace, so it re-parses.
fn pretty_tokens(stream: &TokenStream) -> (String, Vec<Option<usize>>) {
    let mut pretty = Pretty {
        out: String::new(),
        map: vec![None],
    };
    pretty.write(stream.clone(), 0);
    (pretty.out, pretty.map)
}

/// Builds the pretty-printed text and, in lock-step, the original source line of
/// each output line.
struct Pretty {
    out: String,
    /// `map[i]` is the smallest original source line of the tokens on output line
    /// `i` (0-based), or `None` if none of them carried a location.
    map: Vec<Option<usize>>,
}

impl Pretty {
    /// Records that a token from original source line `line` (0 = no location) is
    /// on the current output line.
    fn note(&mut self, line: usize) {
        if line != 0 {
            let i = self.map.len() - 1;
            self.map[i] = Some(self.map[i].map_or(line, |e| e.min(line)));
        }
    }

    /// Starts a new output line indented to `depth`.
    fn newline(&mut self, depth: usize) {
        self.out.push('\n');
        for _ in 0..depth {
            self.out.push_str("    ");
        }
        self.map.push(None);
    }

    fn write(&mut self, stream: TokenStream, depth: usize) {
        for tt in stream {
            self.note(tt.span().start().line);
            match tt {
                TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => {
                    self.out.push('{');
                    self.newline(depth + 1);
                    self.write(group.stream(), depth + 1);
                    self.newline(depth);
                    self.out.push('}');
                    self.newline(depth);
                }
                TokenTree::Group(group) => {
                    let (open, close) = match group.delimiter() {
                        Delimiter::Parenthesis => ('(', ')'),
                        Delimiter::Bracket => ('[', ']'),
                        Delimiter::Brace | Delimiter::None => ('\0', '\0'),
                    };
                    if open != '\0' {
                        self.out.push(open);
                    }
                    self.write(group.stream(), depth);
                    if close != '\0' {
                        self.out.push(close);
                    }
                    self.out.push(' ');
                }
                TokenTree::Ident(ident) => {
                    self.out.push_str(&ident.to_string());
                    self.out.push(' ');
                }
                TokenTree::Literal(lit) => {
                    self.out.push_str(&lit.to_string());
                    self.out.push(' ');
                }
                TokenTree::Punct(punct) => match punct.as_char() {
                    ';' => {
                        self.out.push(';');
                        self.newline(depth);
                    }
                    // The lead-in of an attribute: no space before the `[` or `!`.
                    '#' => self.out.push('#'),
                    c => {
                        self.out.push(c);
                        if punct.spacing() == Spacing::Alone {
                            self.out.push(' ');
                        }
                    }
                },
            }
        }
    }
}

/// Rewrites `rustc`'s `<anon>:LINE:COL` references (`<anon>` is the stdin source)
/// to the user's real `file:line:col`, using the generated-to-original line `map`
/// from [`render_source`]. The column is `rustc`'s (into the generated source), so
/// it is approximate; a line with no mapping keeps `<anon>`. A no-op when the map
/// is empty (an unmappable subcrate).
fn remap_diagnostics(stderr: &str, map: &[Option<usize>], file: &str) -> String {
    const MARKER: &str = "<anon>:";
    if map.is_empty() {
        return stderr.to_string();
    }
    let mut out = String::with_capacity(stderr.len());
    let mut rest = stderr;
    while let Some(pos) = rest.find(MARKER) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + MARKER.len()..];
        let digits = after.chars().take_while(char::is_ascii_digit).count();
        match after[..digits].parse::<usize>() {
            Ok(gline) if map.get(gline).copied().flatten().is_some() => {
                let oline = map[gline].unwrap();
                out.push_str(file);
                out.push(':');
                out.push_str(&oline.to_string());
                rest = &after[digits..]; // keep the ":col…" tail
            }
            // No digits, or an unmapped line: leave `<anon>:` as written.
            _ => {
                out.push_str(MARKER);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The result of [`Config::prepare_subcrate`]: either the subcrate is up to date,
/// or there is work to run.
enum Prepared {
    /// Reuse (the stamp matched): the cached warning text to re-emit, if any.
    Reused(Option<String>),
    /// A job to run on a worker thread.
    Compile(Box<CompileJob>),
}

/// A subcrate's state while [`Config::compile_all`] works through the dependency
/// order.
#[derive(Clone, Copy, PartialEq)]
enum Status {
    Pending,
    Done,
    Failed,
    /// A `#[rustversion]`-gated subcrate that did not build on this toolchain (or
    /// depends on one that did not): not an error, but no artifact exists. Recorded
    /// in `compile_all`'s `gate_skipped` mask. See [`is_rustversion_gate`].
    Skipped,
}

/// Writes the compiler error that made a `#[rustversion]`-gated subcrate fail to
/// build, so the attribute macro can show it. The macro reads
/// `<OUT_DIR>/<internal>.skipreason` when the manifest marks the subcrate skipped
/// (see `subcrate_core::ManifestEntry::Skipped`); a write failure only costs that
/// detail, so it is ignored.
fn write_skip_reason(out_dir: &Path, internal: &str, reason: &str) {
    let _ = std::fs::write(out_dir.join(format!("{}.skipreason", internal)), reason);
}

/// A spawned compile: the worker returns the warnings to emit (`None` if none) or
/// the failure message.
type JobHandle = std::thread::JoinHandle<Result<Option<String>, String>>;

/// Everything a worker thread needs to compile one subcrate: all owned and `Send`,
/// with no borrow of `Config` or any syn data (those are not `Send`, so all of
/// that work is done up front in [`Config::prepare_subcrate`]).
struct CompileJob {
    rustc: PathBuf,
    args: Vec<String>,
    source: String,
    out_dir: PathBuf,
    stamp: PathBuf,
    fingerprint: String,
    warnings_file: PathBuf,
    display_name: String,
    proc_macro: bool,
    /// Generated line → original source line, for rewriting `<anon>` diagnostics
    /// (see [`render_source`]). Empty when the subcrate is not mappable.
    line_map: Vec<Option<usize>>,
    /// The user's source file the diagnostics are rewritten to name.
    source_file: String,
    /// UI-test expectations (see [`run_ui_check`]). Empty (`!ui.is_ui_test()`) for
    /// an ordinary subcrate, which is compiled and reported the plain way.
    ui: UiExpectations,
}

impl CompileJob {
    /// Writes the reuse stamp, so the next build can skip this subcrate while its
    /// inputs are unchanged. Called once a compile (or UI check) has succeeded.
    fn write_stamp(&self) -> Result<(), String> {
        std::fs::write(&self.stamp, &self.fingerprint)
            .map_err(|e| format!("failed to write stamp `{}`: {}", self.stamp.display(), e))
    }
}

/// Runs `rustc` for one prepared subcrate (on a worker thread): feeds the source
/// in on stdin, and on success writes the reuse stamp and caches the warnings.
/// Returns the warning text to emit (`None` if there was none) on success, or the
/// full failure message as `Err`.
fn run_compile_job(job: &CompileJob) -> Result<Option<String>, String> {
    let mut cmd = Command::new(&job.rustc);
    // Read the source from stdin. rustc names it `<anon>`, which is the same in
    // every build — including the separate nested build of the owner crate — so a
    // subcrate compiled in two places keeps one identity. (A per-`OUT_DIR` file
    // path would differ between those builds and break that match.) The source is
    // pretty-printed, so `<anon>:<line>:<col>` still points at the right construct.
    cmd.arg("-");
    for arg in &job.args {
        cmd.arg(arg);
    }
    cmd.arg("--out-dir").arg(&job.out_dir);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return Err(format!("failed to launch `{}`: {}", job.rustc.display(), e)),
    };
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(job.source.as_bytes()) {
            return Err(format!(
                "failed to send source to `{}`: {}",
                job.rustc.display(),
                e
            ));
        }
        // Dropping `stdin` closes the pipe so rustc sees EOF.
    }
    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(e) => return Err(format!("failed to run `{}`: {}", job.rustc.display(), e)),
    };

    // A UI test checks the diagnostics against its `should_*` expectations rather
    // than passing or failing on the exit status alone.
    if job.ui.is_ui_test() {
        return run_ui_check(job, output.status.success(), &output.stderr);
    }

    if !output.status.success() {
        // Rewrite `<anon>:L:C` to the user's real file:line before reporting.
        let stderr = remap_diagnostics(
            &String::from_utf8_lossy(&output.stderr),
            &job.line_map,
            &job.source_file,
        );
        return Err(format!(
            "failed to compile subcrate `{}`{}:\n{}",
            job.display_name,
            if job.proc_macro { " (proc-macro)" } else { "" },
            indent(stderr.trim_end()),
        ));
    }

    // rustc's stderr on success is its warnings (and notes). Cargo hides a build
    // script's own stderr, so the caller forwards each line as a `cargo:warning=`;
    // cache it beside the stamp so a later reused build can re-emit it. Rewrite the
    // `<anon>` locations first, so the cache (and the re-emit) carry real file:line.
    let stderr = remap_diagnostics(
        &String::from_utf8_lossy(&output.stderr),
        &job.line_map,
        &job.source_file,
    );
    let stderr = stderr.trim();
    let warnings = if stderr.is_empty() {
        let _ = std::fs::remove_file(&job.warnings_file);
        None
    } else {
        let _ = std::fs::write(&job.warnings_file, stderr);
        Some(stderr.to_string())
    };

    job.write_stamp()?;
    Ok(warnings)
}

/// Verifies a UI-test subcrate's diagnostics against its `should_*` expectations.
///
/// `stderr` is rustc's `--error-format=json` output (one diagnostic object per
/// line). On a pass, writes the reuse stamp and returns `Ok(None)`: a UI test's
/// own diagnostics are consumed by the assertions, not forwarded as build
/// warnings. On a failure, returns a message listing every unmet expectation next
/// to what actually happened — rustc's own rendered diagnostics, with `<anon>`
/// locations rewritten to the user's real file (see [`remap_diagnostics`]).
fn run_ui_check(job: &CompileJob, success: bool, stderr: &[u8]) -> Result<Option<String>, String> {
    let stderr = String::from_utf8_lossy(stderr);
    let diags = parse_rustc_json(&stderr);
    let problems = check_ui(
        success,
        &diags,
        &job.ui.should_errors,
        &job.ui.should_warns,
        &job.ui.should_spanned,
    );

    if problems.is_empty() {
        job.write_stamp()?;
        return Ok(None);
    }

    let human = remap_diagnostics(&diags.rendered, &job.line_map, &job.source_file);
    let mut message = format!(
        "subcrate `{}`{} did not match its `#[subcrate(…)]` UI-test expectations:\n{}",
        job.display_name,
        if job.proc_macro { " (proc-macro)" } else { "" },
        indent(&problems.join("\n")),
    );
    if human.trim().is_empty() {
        message.push_str("\n\n(the subcrate compiled with no diagnostics)");
    } else {
        message.push_str("\n\nactual diagnostics:\n");
        message.push_str(&indent(human.trim_end()));
    }
    Err(message)
}

/// Diagnostics parsed from rustc's JSON output, as [`run_ui_check`] needs them.
struct UiDiagnostics {
    /// The rendered (human) text of each error-level diagnostic.
    errors: Vec<String>,
    /// The rendered (human) text of each warning-level diagnostic.
    warnings: Vec<String>,
    /// The code underlined by each span (its fragments joined; see
    /// [`collect_highlights`]).
    highlights: Vec<String>,
    /// Every diagnostic's rendered text joined — i.e. rustc's normal human output.
    rendered: String,
}

/// Checks the compile's `success` and parsed `diags` against the expectations,
/// returning the unmet ones (empty = the UI test passed).
///
/// Every expectation must hold (they are ANDed): each `should_error`/`should_warn`
/// substring must appear in some error/warning diagnostic, and each `should_spanned`
/// must match some underlined span. A substring may match any single diagnostic;
/// different substrings need not match the same one.
fn check_ui(
    success: bool,
    diags: &UiDiagnostics,
    should_errors: &[Option<String>],
    should_warns: &[Option<String>],
    should_spanned: &[String],
) -> Vec<String> {
    let mut problems = Vec::new();
    let expects_error = !should_errors.is_empty();

    // If the compile went the wrong way, that is the whole story: the message
    // substring and span checks below would only add misleading noise (an
    // error-test that compiled has no errors to match, and so on). Report just the
    // direction; `run_ui_check` appends what the compiler actually said.
    if expects_error && success {
        return vec![
            "expected compilation to fail, but the subcrate compiled successfully".to_string(),
        ];
    }
    if !expects_error && !success {
        return vec![
            "expected the subcrate to compile (only `should_warn`/`should_spanned` were given), \
             but it failed to compile"
                .to_string(),
        ];
    }

    // A bare `should_error` (`None`) is satisfied by the failure already checked
    // above; only a `Some(substring)` needs a matching error. The substring is
    // matched with whitespace collapsed on both sides (see [`collapse_whitespace`]),
    // so a one-line expectation still matches a message `rustc` hard-wrapped across
    // lines; the author's original spelling is kept for the report.
    for want in should_errors.iter().flatten() {
        let needle = collapse_whitespace(want);
        if !diags
            .errors
            .iter()
            .any(|r| collapse_whitespace(r).contains(&needle))
        {
            problems.push(format!("no error diagnostic contained `{}`", want));
        }
    }
    for want in should_warns {
        match want {
            Some(want) => {
                let needle = collapse_whitespace(want);
                if !diags
                    .warnings
                    .iter()
                    .any(|r| collapse_whitespace(r).contains(&needle))
                {
                    problems.push(format!("no warning diagnostic contained `{}`", want));
                }
            }
            // A bare `should_warn`: any warning satisfies it.
            None => {
                if diags.warnings.is_empty() {
                    problems
                        .push("expected at least one warning, but none were emitted".to_string());
                }
            }
        }
    }
    for want in should_spanned {
        let norm = normalize_spanned(want);
        if !diags
            .highlights
            .iter()
            .any(|h| normalize_spanned(h) == norm)
        {
            problems.push(format!("no diagnostic underlined code equal to `{}`", want));
        }
    }
    problems
}

/// Normalizes code for a `should_spanned` comparison by removing all whitespace,
/// so differences in spacing, indentation and line breaks between the asserted
/// code and the underlined code do not matter.
fn normalize_spanned(code: &str) -> String {
    code.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Collapses every run of whitespace (spaces, tabs and line breaks) to a single
/// space and trims the ends, for the `should_error`/`should_warn` substring match.
/// `rustc` hard-wraps long messages, so a line break can fall in the middle of a
/// sentence; collapsing both the diagnostic and the expected string makes a
/// one-line expectation match across such a wrap. Unlike [`normalize_spanned`] it
/// keeps single spaces, so word boundaries in the message are preserved.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parses rustc's `--error-format=json` stderr into [`UiDiagnostics`].
///
/// Each non-empty line is one JSON diagnostic object; a line that does not parse
/// as an object is skipped, so stray non-JSON output is tolerated. Only top-level
/// diagnostics carry a non-null `rendered` field, so the error/warning text is
/// read from there, while the underlined spans are gathered from the whole tree.
fn parse_rustc_json(stderr: &str) -> UiDiagnostics {
    let mut diags = UiDiagnostics {
        errors: Vec::new(),
        warnings: Vec::new(),
        highlights: Vec::new(),
        rendered: String::new(),
    };
    for line in stderr.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value = match parse_json(line) {
            Some(value) => value,
            None => continue,
        };
        let level = value.get("level").and_then(Json::as_str).unwrap_or("");
        if let Some(rendered) = value.get("rendered").and_then(Json::as_str) {
            if !diags.rendered.is_empty() {
                diags.rendered.push('\n');
            }
            diags.rendered.push_str(rendered);
            if level.starts_with("error") {
                diags.errors.push(rendered.to_string());
            } else if level.starts_with("warning") {
                diags.warnings.push(rendered.to_string());
            }
        }
        collect_highlights(&value, &mut diags.highlights);
    }
    diags
}

/// Walks a parsed diagnostic and collects the code underlined by each span.
///
/// A span records its underlined source as a `"text"` array of
/// `{ "text": <source line>, "highlight_start": c, "highlight_end": c }` fragments;
/// one span's highlighted code is its fragments' `[highlight_start, highlight_end)`
/// slices (1-based columns) joined. Recurses through `children` and `spans` so a
/// note's or suggestion's underline is collected too.
fn collect_highlights(value: &Json, out: &mut Vec<String>) {
    match value {
        Json::Obj(fields) => {
            if let Some(Json::Arr(texts)) = value.get("text") {
                let mut fragment = String::new();
                let mut any = false;
                for text in texts {
                    let line = text.get("text").and_then(Json::as_str);
                    let start = text.get("highlight_start").and_then(Json::as_num);
                    let end = text.get("highlight_end").and_then(Json::as_num);
                    if let (Some(line), Some(start), Some(end)) = (line, start, end) {
                        let start = (start as usize).saturating_sub(1);
                        let end = (end as usize).saturating_sub(1);
                        if end > start {
                            fragment.extend(line.chars().skip(start).take(end - start));
                            any = true;
                        }
                    }
                }
                if any {
                    out.push(fragment);
                }
            }
            for (_, child) in fields {
                collect_highlights(child, out);
            }
        }
        Json::Arr(items) => {
            for item in items {
                collect_highlights(item, out);
            }
        }
        _ => {}
    }
}

/// A parsed JSON value — just enough of the data model for rustc's diagnostics.
/// `true`, `false` and `null` all parse to [`Json::Lit`]: this parser never reads
/// a boolean or null value, only skips past it to reach the fields it wants.
enum Json {
    Lit,
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// The value of object field `key`, if this is an object that has it.
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn as_num(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }
}

/// Parses a single JSON value from `input`, or `None` on malformed input or if
/// anything but whitespace follows the value. A small hand-written parser keeps
/// the dependency surface (and MSRV) small; the input is one rustc diagnostic.
fn parse_json(input: &str) -> Option<Json> {
    let mut parser = JsonParser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    parser.skip_ws();
    let value = parser.value()?;
    parser.skip_ws();
    if parser.pos == parser.bytes.len() {
        Some(value)
    } else {
        None
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl JsonParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn skip_ws(&mut self) {
        while matches!(
            self.peek(),
            Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')
        ) {
            self.pos += 1;
        }
    }

    fn value(&mut self) -> Option<Json> {
        self.skip_ws();
        match self.peek()? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => self.string().map(Json::Str),
            b't' => self.literal("true", Json::Lit),
            b'f' => self.literal("false", Json::Lit),
            b'n' => self.literal("null", Json::Lit),
            _ => self.number(),
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Option<Json> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Some(value)
        } else {
            None
        }
    }

    fn object(&mut self) -> Option<Json> {
        self.bump(); // '{'
        let mut fields = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.bump();
            return Some(Json::Obj(fields));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return None;
            }
            let key = self.string()?;
            self.skip_ws();
            if self.bump() != Some(b':') {
                return None;
            }
            let value = self.value()?;
            fields.push((key, value));
            self.skip_ws();
            match self.bump()? {
                b',' => continue,
                b'}' => return Some(Json::Obj(fields)),
                _ => return None,
            }
        }
    }

    fn array(&mut self) -> Option<Json> {
        self.bump(); // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.bump();
            return Some(Json::Arr(items));
        }
        loop {
            let value = self.value()?;
            items.push(value);
            self.skip_ws();
            match self.bump()? {
                b',' => continue,
                b']' => return Some(Json::Arr(items)),
                _ => return None,
            }
        }
    }

    /// Reads a string starting at the opening quote, decoding escapes via the
    /// shared [`json_string`] reader and advancing past the closing quote.
    fn string(&mut self) -> Option<String> {
        if self.bump() != Some(b'"') {
            return None;
        }
        let rest = std::str::from_utf8(&self.bytes[self.pos..]).ok()?;
        let (value, after) = json_string(rest)?;
        self.pos += rest.len() - after.len();
        Some(value)
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.pos;
        while matches!(
            self.peek(),
            Some(b'0'..=b'9') | Some(b'-') | Some(b'+') | Some(b'.') | Some(b'e') | Some(b'E')
        ) {
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
        text.parse::<f64>().ok().map(Json::Num)
    }
}

/// Reads the package's edition from `Cargo.toml`, following
/// `edition.workspace = true` up to the workspace root. Falls back to `2015`
/// (Cargo's default) when nothing is specified.
///
/// We parse the handful of lines we need by hand rather than pulling in a full
/// TOML parser: it keeps the dependency surface (and MSRV) small, and the
/// `edition` key we care about has a simple, well-understood shape.
fn detect_edition(manifest_dir: &Path) -> String {
    const DEFAULT: &str = "2015";
    let text = match std::fs::read_to_string(manifest_dir.join("Cargo.toml")) {
        Ok(text) => text,
        Err(_) => return DEFAULT.to_string(),
    };

    match find_edition(&text, "package") {
        EditionField::Value(s) => s,
        EditionField::Inherited => {
            workspace_edition(manifest_dir).unwrap_or_else(|| DEFAULT.to_string())
        }
        EditionField::Missing => DEFAULT.to_string(),
    }
}

/// Walks up from `start`, looking for a `Cargo.toml` that defines
/// `[workspace.package] edition = "…"`, to resolve `edition.workspace = true`.
fn workspace_edition(start: &Path) -> Option<String> {
    let mut dir = Some(start.to_path_buf());
    while let Some(d) = dir {
        if let Ok(text) = std::fs::read_to_string(d.join("Cargo.toml")) {
            if let EditionField::Value(s) = find_edition(&text, "workspace.package") {
                return Some(s);
            }
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    None
}

enum EditionField {
    /// `edition = "2021"`
    Value(String),
    /// `edition.workspace = true` / `edition = { workspace = true }`
    Inherited,
    Missing,
}

/// Finds the `edition` key inside the `[section]` table of a `Cargo.toml`.
fn find_edition(manifest: &str, section: &str) -> EditionField {
    let mut in_section = false;
    for raw in manifest.lines() {
        let line = strip_toml_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_section = line[1..line.len() - 1].trim() == section;
            continue;
        }
        if !in_section {
            continue;
        }
        let rest = match line.strip_prefix("edition") {
            Some(rest) => rest.trim_start(),
            None => continue,
        };
        if let Some(value) = rest.strip_prefix('=') {
            // `edition = "2021"` or `edition = { workspace = true }`
            let value = value.trim();
            if let Some(s) = toml_string_value(value) {
                return EditionField::Value(s);
            }
            if value.starts_with('{') && value.contains("workspace") && value.contains("true") {
                return EditionField::Inherited;
            }
        } else if let Some(after) = rest.strip_prefix(".workspace") {
            // `edition.workspace = true`
            if after.trim_start().trim_start_matches('=').trim() == "true" {
                return EditionField::Inherited;
            }
        }
    }
    EditionField::Missing
}

/// The characters of `text` that are outside string literals, with their byte
/// offsets. (A string literal's quotes and contents are left out.)
fn unquoted(text: &str) -> Vec<(usize, char)> {
    let mut chars = Vec::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (at, c) in text.char_indices() {
        match quote {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' && q == '"' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
            None if c == '"' || c == '\'' => quote = Some(c),
            None => chars.push((at, c)),
        }
    }
    chars
}

/// Removes a trailing `# comment` (good enough for the simple lines we read).
fn strip_toml_comment(line: &str) -> &str {
    match unquoted(line).into_iter().find(|&(_, c)| c == '#') {
        Some((at, _)) => &line[..at],
        None => line,
    }
}

/// Extracts the contents of a leading `"…"` (or `'…'`) string literal, if present.
fn toml_string_value(value: &str) -> Option<String> {
    let value = value.trim();
    let quote = value.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let rest = &value[1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

/// Splits `key = value` at the first `=` outside a string.
fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let (at, _) = unquoted(line).into_iter().find(|&(_, c)| c == '=')?;
    Some((&line[..at], &line[at + 1..]))
}

/// Splits `text` at the commas that are neither in a string nor in brackets.
fn split_top_level(text: &str) -> Vec<&str> {
    let mut fields = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (at, c) in unquoted(text) {
        match c {
            '[' | '{' => depth += 1,
            ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                fields.push(&text[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    fields.push(&text[start..]);
    fields
}

/// The parts of a TOML (dotted) key: `a."b.c".d` is `["a", "b.c", "d"]`. `None` if
/// it is not a key.
fn split_toml_key(key: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut rest = key.trim();
    loop {
        let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'');
        let (part, after) = match quote {
            Some(quote) => {
                let body = &rest[1..];
                let end = body.find(quote)?;
                (&body[..end], &body[end + 1..])
            }
            None => {
                let end = rest
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                    .unwrap_or(rest.len());
                if end == 0 {
                    return None;
                }
                (&rest[..end], &rest[end..])
            }
        };
        parts.push(part.to_string());
        rest = after.trim_start();
        if rest.is_empty() {
            return Some(parts);
        }
        rest = rest.strip_prefix('.')?.trim_start();
    }
}

/// The raw value of the top-level `key` in an inline table `{ a = 1, key = "x" }`.
fn inline_table_field<'a>(table: &'a str, key: &str) -> Option<&'a str> {
    let inner = table.trim().strip_prefix('{')?.strip_suffix('}')?;
    split_top_level(inner).into_iter().find_map(|field| {
        let (name, value) = split_key_value(field)?;
        match split_toml_key(name)?.as_slice() {
            [only] if only == key => Some(value.trim()),
            _ => None,
        }
    })
}

/// Which table of a `Cargo.toml` a dependency is declared in. Ordered by how much
/// the package's ordinary code is entitled to use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DependencyTable {
    /// `[dependencies]`, also under `[target.<cfg>.…]`.
    Normal,
    /// `[dev-dependencies]`.
    Dev,
    /// `[build-dependencies]`.
    Build,
    /// `[workspace.dependencies]`, which members inherit with `workspace = true`.
    Workspace,
}

/// A dependency declared in a `Cargo.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeclaredDep {
    table: DependencyTable,
    /// The key it is declared under: the name the package's own code refers to the
    /// crate by (with `-` as `_`).
    key: String,
    /// The real package, if the dependency is renamed: `package = "…"`.
    package: Option<String>,
    /// Whether it is inherited from the workspace: `workspace = true`.
    workspace: bool,
    /// The version requirement: `version = "1"`, or the plain `name = "1"`.
    version: Option<String>,
    /// Where it is on disk: `path = "…"`.
    path: Option<String>,
    /// Which table of the file (counting headers) it is in, to keep apart the same
    /// key in two `[target.…]` tables.
    section: usize,
}

/// What a table header of a `Cargo.toml` opens, as far as dependencies go.
enum Section {
    Other,
    /// A table of dependencies, one `key = …` each.
    Dependencies(DependencyTable),
    /// The table of a single dependency, `[dependencies.key]`.
    Dependency(DependencyTable, String),
}

/// Reads a table header (`[dependencies]`, `[target.'cfg(unix)'.dev-dependencies]`,
/// `[dependencies.foo]`, …).
fn dependency_section(header: &str) -> Section {
    if header.starts_with("[[") || !header.ends_with(']') {
        return Section::Other;
    }
    let keys = match split_toml_key(&header[1..header.len() - 1]) {
        Some(keys) => keys,
        None => return Section::Other,
    };
    let kind = |key: &str| match key {
        "dependencies" => Some(DependencyTable::Normal),
        "dev-dependencies" | "dev_dependencies" => Some(DependencyTable::Dev),
        "build-dependencies" | "build_dependencies" => Some(DependencyTable::Build),
        _ => None,
    };
    let (table, rest) = match keys.as_slice() {
        [workspace, dependencies, rest @ ..]
            if workspace == "workspace" && dependencies == "dependencies" =>
        {
            (DependencyTable::Workspace, rest)
        }
        [target, _, key, rest @ ..] if target == "target" => match kind(key) {
            Some(table) => (table, rest),
            None => return Section::Other,
        },
        [key, rest @ ..] => match kind(key) {
            Some(table) => (table, rest),
            None => return Section::Other,
        },
        [] => return Section::Other,
    };
    match rest {
        [] => Section::Dependencies(table),
        [name] => Section::Dependency(table, name.clone()),
        _ => Section::Other,
    }
}

/// The dependencies a `Cargo.toml` declares, with whether each is renamed or
/// inherited. It reads `[dependencies]`, `[dev-dependencies]` and
/// `[build-dependencies]` (including under `[target.…]`), and
/// `[workspace.dependencies]`.
///
/// This is a line-by-line reading, like [`find_edition`]: a dependency is
/// `name = "1"`, `name = { package = "real", … }` on one line, `name.package =
/// "real"`, or a `[dependencies.name]` table. (A TOML parser is not worth the
/// dependency.)
fn declared_dependencies(manifest: &str) -> Vec<DeclaredDep> {
    fn entry<'a>(
        deps: &'a mut Vec<DeclaredDep>,
        table: DependencyTable,
        key: &str,
        section: usize,
    ) -> &'a mut DeclaredDep {
        let at = deps
            .iter()
            .position(|dep| dep.table == table && dep.key == key && dep.section == section);
        let at = at.unwrap_or_else(|| {
            deps.push(DeclaredDep {
                table,
                key: key.to_string(),
                package: None,
                workspace: false,
                version: None,
                path: None,
                section,
            });
            deps.len() - 1
        });
        &mut deps[at]
    }
    fn set_field(dep: &mut DeclaredDep, field: &str, value: &str) {
        match field {
            "package" => dep.package = toml_string_value(value),
            "workspace" => dep.workspace = value.trim() == "true",
            "version" => dep.version = toml_string_value(value),
            "path" => dep.path = toml_string_value(value),
            _ => {}
        }
    }

    let mut deps: Vec<DeclaredDep> = Vec::new();
    let mut section = Section::Other;
    let mut headers = 0;
    for raw in manifest.lines() {
        let line = strip_toml_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = dependency_section(line);
            headers += 1;
            continue;
        }
        let (key, value) = match split_key_value(line) {
            Some(pair) => pair,
            None => continue,
        };
        let keys = match split_toml_key(key) {
            Some(keys) => keys,
            None => continue,
        };
        match &section {
            Section::Other => {}
            Section::Dependencies(table) => {
                let dep = entry(&mut deps, *table, &keys[0], headers);
                match &keys[1..] {
                    [] if value.trim().starts_with('{') => {
                        // `name = { package = "real", … }`
                        for field in ["package", "workspace", "version", "path"] {
                            if let Some(value) = inline_table_field(value, field) {
                                set_field(dep, field, value);
                            }
                        }
                    }
                    // `name = "1"`
                    [] => set_field(dep, "version", value),
                    // `name.package = "real"`
                    [field] => set_field(dep, field, value),
                    _ => {}
                }
            }
            Section::Dependency(table, name) => {
                if let [field] = keys.as_slice() {
                    set_field(entry(&mut deps, *table, name, headers), field, value);
                }
            }
        }
    }
    deps
}

/// The entry of `declared` that the crate name `name` refers to: the dependency
/// declared under that key (`-` and `_` being one), the ordinary ones before the
/// dev- and build-dependencies. `None` if there is no such dependency.
fn find_declared<'a>(declared: &'a [DeclaredDep], name: &str) -> Option<&'a DeclaredDep> {
    let in_scope = || {
        declared
            .iter()
            .filter(|dep| dep.table != DependencyTable::Workspace)
    };
    let best = |matches: Vec<&'a DeclaredDep>| matches.into_iter().min_by_key(|dep| dep.table);
    best(in_scope().filter(|dep| dep.key == name).collect()).or_else(|| {
        best(
            in_scope()
                .filter(|dep| dep.key.replace('-', "_") == name)
                .collect(),
        )
    })
}

/// The dependency the workspace declares under `key` in `[workspace.dependencies]`,
/// found by walking up from `start` to the first `Cargo.toml` that has it.
fn workspace_dependency(start: &Path, key: &str) -> Option<DeclaredDep> {
    let mut dir = Some(start.to_path_buf());
    while let Some(d) = dir {
        if let Ok(text) = std::fs::read_to_string(d.join("Cargo.toml")) {
            let found = declared_dependencies(&text)
                .into_iter()
                .find(|dep| dep.table == DependencyTable::Workspace && dep.key == key);
            if found.is_some() {
                return found;
            }
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    None
}

/// The `[package]` version of a `Cargo.toml`, if it is spelled out there.
fn package_version(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for raw in manifest.lines() {
        let line = strip_toml_comment(raw).trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_package = line[1..line.len() - 1].trim() == "package";
        } else if in_package {
            if let Some((key, value)) = split_key_value(line) {
                if key.trim() == "version" {
                    return toml_string_value(value);
                }
            }
        }
    }
    None
}

/// What the owner's `Cargo.toml` says `::name` is.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Declared {
    /// The real package.
    package: String,
    /// What tells its versions apart, if the dependency graph holds several.
    pin: Option<Pin>,
}

/// How a declared dependency picks its version.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pin {
    /// The version of the package on disk that a `path` dependency points to.
    Exact(String),
    /// The version requirement, `version = "1.2"`.
    Requirement(String),
}

impl Pin {
    /// Whether the locked `version` is the one this pins.
    fn matches(&self, version: &str) -> bool {
        match self {
            Pin::Exact(exact) => exact == version,
            Pin::Requirement(requirement) => version_matches(requirement, version),
        }
    }
}

/// The numeric components of a version or a partial one (`1`, `1.2`, `1.2.3`, `1.*`,
/// `*`; a pre-release or build suffix is ignored). `None` if it is not one.
fn version_components(text: &str) -> Option<Vec<u64>> {
    let text = text.trim();
    let text = text.split(|c| c == '-' || c == '+').next().unwrap_or(text);
    let mut components = Vec::new();
    for part in text.split('.') {
        match part.trim() {
            "*" | "x" | "X" | "" => break,
            number => components.push(number.parse().ok()?),
        }
    }
    if components.len() > 3 {
        return None;
    }
    Some(components)
}

/// Whether `version` satisfies the Cargo version requirement `requirement`:
/// comma-separated comparators, each an optional operator (`^`, `~`, `=`, `<`, `<=`,
/// `>`, `>=`; `^` is the default) and a possibly partial version or a wildcard.
fn version_matches(requirement: &str, version: &str) -> bool {
    let version = match version_components(version) {
        Some(components) if components.len() == 3 => [components[0], components[1], components[2]],
        _ => return false,
    };
    requirement
        .split(',')
        .all(|comparator| comparator_matches(comparator.trim(), version))
}

/// One comparator of [`version_matches`].
fn comparator_matches(comparator: &str, version: [u64; 3]) -> bool {
    let operators = ["<=", ">=", "<", ">", "=", "^", "~"];
    let operator = operators
        .iter()
        .find(|operator| comparator.starts_with(**operator))
        .copied();
    let rest = &comparator[operator.map_or(0, str::len)..];
    let given = match version_components(rest) {
        Some(given) => given,
        None => return false,
    };
    // The first version past everything that starts with the first `n` components
    // of `given` (`1.2` with `n` 2: `1.3.0`).
    let past = |n: usize| {
        let mut bound = [0; 3];
        bound[..n].copy_from_slice(&given[..n]);
        bound[n - 1] += 1;
        bound
    };
    let mut lower = [0; 3];
    lower[..given.len()].copy_from_slice(&given);
    if given.is_empty() {
        // A bare `*`: anything (but not `<*`, which is nothing).
        return !matches!(operator, Some("<") | Some(">"));
    }
    let wildcard = rest.contains(|c| c == '*' || c == 'x' || c == 'X');
    match operator {
        Some("<") => version < lower,
        Some("<=") => version < past(given.len()),
        Some(">") => version >= past(given.len()),
        Some(">=") => version >= lower,
        Some("=") => version >= lower && version < past(given.len()),
        Some("~") => version >= lower && version < past(if given.len() == 1 { 1 } else { 2 }),
        _ if wildcard => version >= lower && version < past(given.len()),
        // `^`, and a comparator with none: compatible up to the leftmost non-zero
        // component given (or the last one, if there is none).
        _ => {
            let at = given
                .iter()
                .position(|&n| n != 0)
                .unwrap_or(given.len() - 1);
            version >= lower && version < past(at + 1)
        }
    }
}

/// Removes artifacts in `dir` left over from subcrate bodies that no longer
/// exist (their content-addressed name changed).
fn prune_stale(dir: &Path, keep: &[String]) {
    let keep: HashSet<&str> = keep.iter().map(String::as_str).collect();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // File names are `lib<internal>.rlib`/`.rmeta`/`.so`/… for compiler
        // outputs and `<internal>.stamp` for our marker. Strip the optional
        // `lib` prefix, then everything from the first `.` is the extension.
        let base = name.strip_prefix("lib").unwrap_or(&name);
        if !base.starts_with(subcrate_core::INTERNAL_PREFIX) {
            continue;
        }
        let internal = base.split('.').next().unwrap_or("");
        if !keep.contains(internal) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The crates built by Cargo for the subcrates that use them: the owner's
/// dependencies, their own dependencies, and possibly the owner crate.
#[derive(Debug, Clone, Default)]
struct OwnerDeps {
    /// The artifacts of every crate Cargo built, by crate name. (Of a crate built
    /// in several versions, the first one Cargo reported: see `versions`.)
    crates: BTreeMap<String, OwnerCrate>,
    /// The same artifacts by crate name *and* version, for telling apart the
    /// versions of a crate that the dependency graph holds several of.
    versions: BTreeMap<(String, String), OwnerCrate>,
    /// What each `::name` the subcrates use stands for.
    externals: BTreeMap<String, ExternalCrate>,
    /// Every directory holding an artifact: search paths for the dependencies'
    /// own dependencies.
    lib_dirs: BTreeSet<PathBuf>,
    /// The library search paths the build scripts of the crates built asked for
    /// (`cargo:rustc-link-search`), as far as they are in the nested build's own
    /// target directory. A crate that is backed by subcrates keeps those in
    /// `OUT_DIR/subcrates`, a directory of none of its artifacts' — and its metadata
    /// refers to them, so whatever compiles against that crate has to be able to find
    /// them (and the crates *they* were built against, which its own nested build
    /// put elsewhere). Cargo gives these paths to the compilations it runs, not to the
    /// subcrates this build script compiles.
    script_paths: BTreeSet<PathBuf>,
    /// The manifests and sources of the path dependencies built, which change
    /// when someone edits them.
    sources: BTreeSet<PathBuf>,
}

/// The artifacts of one crate built by Cargo.
#[derive(Debug, Clone, Default)]
struct OwnerCrate {
    rlib: Option<PathBuf>,
    rmeta: Option<PathBuf>,
    /// The features of the build `rlib`/`rmeta` come from. Under resolver 2 one
    /// version can be built more than once with different features; the build kept
    /// is the one whose feature set is a superset of the others' (see `merge_build`).
    features: Vec<String>,
    /// The features of another build whose set is neither a subset nor a superset
    /// of `features`. When set, which build to link is ambiguous, and linking this
    /// crate is an error rather than a silent wrong choice.
    conflict: Option<Vec<String>>,
}

/// The crate behind a `::name` that a subcrate uses.
#[derive(Debug, Clone)]
struct ExternalCrate {
    /// The library crate (the package's name, `-` as `_`) whose artifacts are
    /// wanted. It differs from the `::name` written when the dependency is renamed.
    crate_name: String,
    /// Which version of it, when the dependency graph holds more than one.
    version: Option<String>,
}

/// Scans the build-output tree under `base` for library artifacts, for a nested run
/// reusing the enclosing build's copies (see [`Config::owner_deps_from_enclosing`]).
/// Returns the newest `lib<crate>-<hash>.rlib` by crate name, and every directory
/// holding an artifact (the `-L` search paths for those crates' own dependencies).
///
/// It walks rather than reads one fixed directory because Cargo's layout varies: a
/// stable toolchain puts a crate's own file in `deps/lib<crate>-<hash>.rlib`, a
/// recent nightly under `build/<pkg>/<hash>/out/`. The crate name runs up to the
/// last `-` (a crate name has no `-`, a hash is hex); should several builds of one
/// crate be left over, the newest is the one Cargo's own linking would pick.
fn scan_build_artifacts(base: &Path) -> (BTreeMap<String, PathBuf>, BTreeSet<PathBuf>) {
    let mut rlibs: BTreeMap<String, (std::time::SystemTime, PathBuf)> = BTreeMap::new();
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                // Skip trees that never hold a linkable artifact.
                match entry.file_name().to_str() {
                    Some("incremental") | Some(".fingerprint") => {}
                    _ => stack.push(entry.path()),
                }
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            match Path::new(&*name).extension().and_then(|ext| ext.to_str()) {
                Some("rlib") | Some("rmeta") | Some("so") | Some("dylib") | Some("dll") => {
                    dirs.insert(dir.clone());
                }
                _ => continue,
            }
            let stem = match name
                .strip_prefix("lib")
                .and_then(|s| s.strip_suffix(".rlib"))
            {
                Some(stem) => stem,
                None => continue,
            };
            let cut = match stem.rfind('-') {
                Some(cut) => cut,
                None => continue,
            };
            let (crate_name, hash) = (&stem[..cut], &stem[cut + 1..]);
            if crate_name.is_empty()
                || hash.is_empty()
                || !hash.bytes().all(|b| b.is_ascii_hexdigit())
            {
                continue;
            }
            let mtime = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            if rlibs
                .get(crate_name)
                .map_or(true, |(best, _)| mtime >= *best)
            {
                rlibs.insert(crate_name.to_string(), (mtime, entry.path()));
            }
        }
    }
    (
        rlibs
            .into_iter()
            .map(|(name, (_, path))| (name, path))
            .collect(),
        dirs,
    )
}

/// Reads the artifacts out of the JSON messages of a `cargo build
/// --message-format=json`. Each `compiler-artifact` message names the crate
/// (`"target":{…"name":"…"…}`) and lists its files (`"filenames":[…]`), for the
/// crates asked for *and* everything they depend on.
///
/// The messages are scanned by hand rather than parsed, as the few fields needed
/// have a fixed shape and a JSON library is not worth the dependency.
fn parse_artifacts(stdout: &str) -> OwnerDeps {
    let mut deps = OwnerDeps::default();
    for line in stdout.lines() {
        if line.contains("\"reason\":\"build-script-executed\"") {
            deps.script_paths.extend(build_script_link_paths(line));
            continue;
        }
        if !line.contains("\"reason\":\"compiler-artifact\"") {
            continue;
        }
        // A crate's name has underscores; older Cargo reports the target's name as
        // the package's (`leaf-base`).
        let name = match line
            .find("\"target\":{")
            .and_then(|at| json_string_field(&line[at..], "name"))
        {
            Some(name) => name.replace('-', "_"),
            None => continue,
        };
        let version = json_string_field(line, "package_id")
            .as_deref()
            .and_then(package_id_version);
        // The features this build was compiled with; empty from older Cargo, which
        // does not report them (then there is only ever one build to choose).
        let features = json_string_array_field(line, "features");
        // One message is one build: collect its rlib and rmeta together, so the
        // feature set can be matched to the right pair.
        let mut rlib = None;
        let mut rmeta = None;
        for file in json_string_array_field(line, "filenames") {
            let file = PathBuf::from(file);
            match file.extension().and_then(|ext| ext.to_str()) {
                Some("rlib") => {
                    rlib.get_or_insert_with(|| file.clone());
                }
                Some("rmeta") => {
                    rmeta.get_or_insert_with(|| file.clone());
                }
                // A proc-macro crate: not linkable here, but a library may depend
                // on it, so its directory has to be searched too.
                Some("so") | Some("dylib") | Some("dll") => {}
                _ => continue,
            }
            if let Some(dir) = file.parent() {
                deps.lib_dirs.insert(dir.to_path_buf());
            }
        }
        if rlib.is_some() || rmeta.is_some() {
            merge_build(
                deps.crates.entry(name.clone()).or_default(),
                rlib.clone(),
                rmeta.clone(),
                &features,
            );
            if let Some(version) = &version {
                let key = (name.clone(), version.clone());
                merge_build(
                    deps.versions.entry(key).or_default(),
                    rlib,
                    rmeta,
                    &features,
                );
            }
        }

        // `path+…` is a package on disk (as opposed to `registry+…` or `git+…`).
        let on_disk =
            json_string_field(line, "package_id").map_or(false, |id| id.contains("path+"));
        if on_disk {
            if let Some(manifest) = json_string_field(line, "manifest_path") {
                let manifest = PathBuf::from(manifest);
                if let Some(dir) = manifest.parent() {
                    for name in ["src", "build.rs"] {
                        let path = dir.join(name);
                        if path.exists() {
                            deps.sources.insert(path);
                        }
                    }
                }
                deps.sources.insert(manifest);
            }
        }
    }
    for krate in deps.crates.values_mut().chain(deps.versions.values_mut()) {
        krate.prefer_hashed_rlib();
    }
    deps
}

/// The library search paths one `build-script-executed` message reports
/// (`"linked_paths":["/…/out/subcrates","dependency=/…/debug"]`), without their
/// kind. A `framework` path is no place for crates and is left out.
fn build_script_link_paths(message: &str) -> Vec<PathBuf> {
    json_string_array_field(message, "linked_paths")
        .into_iter()
        .filter_map(|entry| {
            // `kind=path`, or just `path` (a path may itself contain `=`, so only the
            // kinds Cargo knows are cut off).
            let kinds = ["native", "dependency", "crate", "all"];
            let path = match kinds
                .iter()
                .find_map(|kind| entry.strip_prefix(&format!("{}=", kind)))
            {
                Some(path) => path,
                None if entry.starts_with("framework=") => return None,
                None => entry.as_str(),
            };
            Some(PathBuf::from(path))
        })
        .collect()
}

/// Whether `path` is in the directory `dir`, however either is spelled (symlinks
/// resolved when it is not literally inside).
fn is_inside(path: &Path, dir: &Path) -> bool {
    path.starts_with(dir) || canonical_path(path).starts_with(canonical_path(dir))
}

impl OwnerCrate {
    /// Points the rlib to the one next to the rmeta, if there is one. For the crates
    /// asked for, Cargo reports the rlib under the name it links the crate to
    /// (`target/debug/libdup.rlib`), which belongs to no one crate: two packages of
    /// one name (two versions of `dup`) are linked to the same name, as are the same
    /// package built by two compilers. The file next to the rmeta
    /// (`target/debug/deps/libdup-<hash>.rlib`) is the crate's own.
    fn prefer_hashed_rlib(&mut self) {
        let own = self
            .rmeta
            .as_ref()
            .map(|rmeta| rmeta.with_extension("rlib"));
        if let Some(own) = own.filter(|own| own.is_file()) {
            self.rlib = Some(own);
        }
    }
}

/// Merges one build of a crate into `slot`, keeping the build whose feature set is
/// a superset. Under resolver 2 one version can be built more than once with
/// different features; the subcrate must link the build that has the features it
/// needs. The build with the most features (a superset of the others') is safe:
/// whatever any build offers, it offers too. If two builds' feature sets are
/// incomparable — neither contains the other — the choice is genuinely ambiguous,
/// so `slot` keeps one and records the other in `conflict`, and linking the crate
/// reports it rather than picking the wrong one silently.
fn merge_build(
    slot: &mut OwnerCrate,
    rlib: Option<PathBuf>,
    rmeta: Option<PathBuf>,
    features: &[String],
) {
    if slot.rlib.is_none() && slot.rmeta.is_none() {
        slot.rlib = rlib;
        slot.rmeta = rmeta;
        slot.features = features.to_vec();
        return;
    }
    let current: BTreeSet<&str> = slot.features.iter().map(String::as_str).collect();
    let incoming: BTreeSet<&str> = features.iter().map(String::as_str).collect();
    if incoming == current {
        // The same build, reported across two messages (pipelining emits the rmeta
        // before the rlib): fill in whichever this message carries.
        slot.rlib = slot.rlib.take().or(rlib);
        slot.rmeta = slot.rmeta.take().or(rmeta);
    } else if incoming.is_superset(&current) {
        // Strictly more features: prefer this build. A conflict seen earlier stays
        // recorded (ambiguity, once found, is reported conservatively).
        slot.rlib = rlib;
        slot.rmeta = rmeta;
        slot.features = features.to_vec();
    } else if current.is_superset(&incoming) {
        // The build already kept has every feature this one does: keep it.
    } else if slot.conflict.is_none() {
        slot.conflict = Some(features.to_vec());
    }
}

/// The version in a Cargo package id: `name version (source)` for older Cargo,
/// `source#name@version` (or `source#version`, when the source's last path segment
/// is the name) for newer.
fn package_id_version(id: &str) -> Option<String> {
    let version = if id.contains(' ') {
        id.split(' ').nth(1)?
    } else {
        let fragment = id.rsplit('#').next().filter(|_| id.contains('#'))?;
        fragment.rsplit('@').next()?
    };
    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

/// The (unescaped) value of the first string field `"key":"…"` in `json`.
fn json_string_field(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\":\"", key);
    let at = json.find(&pattern)? + pattern.len();
    json_string(&json[at..]).map(|(value, _)| value)
}

/// The (unescaped) strings of the first array field `"key":["…","…"]` in `json`.
fn json_string_array_field(json: &str, key: &str) -> Vec<String> {
    let pattern = format!("\"{}\":[", key);
    let mut rest = match json.find(&pattern) {
        Some(at) => &json[at + pattern.len()..],
        None => return Vec::new(),
    };
    let mut values = Vec::new();
    loop {
        rest = rest.trim_start_matches(|c: char| c == ',' || c.is_whitespace());
        match rest.strip_prefix('"').and_then(json_string) {
            Some((value, after)) => {
                values.push(value);
                rest = after;
            }
            None => return values,
        }
    }
}

/// Reads a JSON string whose opening quote has been consumed: returns its
/// unescaped value and what follows the closing quote.
fn json_string(text: &str) -> Option<(String, &str)> {
    let mut value = String::new();
    let mut chars = text.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => return Some((value, &text[at + 1..])),
            '\\' => match chars.next()?.1 {
                'n' => value.push('\n'),
                'r' => value.push('\r'),
                't' => value.push('\t'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).map(|(_, c)| c).collect();
                    let c = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32);
                    value.push(c.unwrap_or('\u{fffd}'));
                }
                // `\\`, `\"` and `\/` stand for themselves.
                other => value.push(other),
            },
            c => value.push(c),
        }
    }
    None
}

/// Finds the `Cargo.lock` that governs `manifest_dir`: the nearest one in it or
/// above it. Returns its path and contents.
fn find_lock(manifest_dir: &Path) -> Option<(PathBuf, String)> {
    let mut dir = Some(manifest_dir);
    while let Some(d) = dir {
        let path = d.join("Cargo.lock");
        if let Ok(text) = std::fs::read_to_string(&path) {
            return Some((path, text));
        }
        dir = d.parent();
    }
    None
}

/// One entry of a package's `dependencies` list in the lock file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LockedDep {
    /// The package's name.
    name: String,
    /// Its version, which the lock file spells out only where the name alone would
    /// not say which package is meant (and in lock files of the oldest format,
    /// everywhere).
    version: Option<String>,
    /// How many packages of this name the lock file holds: if more than one,
    /// `cargo build -p <name>` is ambiguous.
    copies: usize,
}

/// The packages `package` depends on (of any kind: normal, build or dev), as
/// listed in the lock file `lock`, or `None` if `package` is not in it.
///
/// A lock file lists each package as a `[[package]]` table whose `name` is
/// followed by a `dependencies` array of `"name"`, `"name version"` or `"name
/// version (source)"` strings.
fn locked_dependencies(lock: &str, package: &str) -> Option<Vec<LockedDep>> {
    let mut found = false;
    let mut in_package = false;
    let mut in_dependencies = false;
    // Every package in the lock file, to count the copies of a dependency.
    let mut names: Vec<&str> = Vec::new();
    let mut dependencies: Vec<(String, Option<String>)> = Vec::new();
    for line in lock.lines().map(str::trim) {
        if line.starts_with("[[") {
            in_package = false;
            in_dependencies = false;
        } else if let Some(name) = line
            .strip_prefix("name = \"")
            .and_then(|rest| rest.split('"').next())
        {
            names.push(name);
            in_package = name == package;
            found |= in_package;
        } else if !in_package {
            continue;
        } else if in_dependencies {
            match line.trim_end_matches(',').trim().strip_prefix('"') {
                Some(entry) => {
                    let entry = entry.split('"').next().unwrap_or(entry);
                    let mut words = entry.split(' ');
                    let name = words.next().unwrap_or(entry).to_string();
                    dependencies.push((name, words.next().map(str::to_string)));
                }
                None => in_dependencies = false,
            }
        } else if line == "dependencies = [" {
            in_dependencies = true;
        }
    }
    if !found {
        return None;
    }
    Some(
        dependencies
            .into_iter()
            .map(|(name, version)| LockedDep {
                copies: names.iter().filter(|other| **other == name).count(),
                name,
                version,
            })
            .collect(),
    )
}

/// A package in a lock file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LockedPackage {
    name: String,
    version: String,
    /// Where it comes from (`registry+…`, `git+…`); a path package has none.
    source: Option<String>,
}

/// Every package of the lock file `lock`.
fn locked_packages(lock: &str) -> Vec<LockedPackage> {
    let mut packages: Vec<LockedPackage> = Vec::new();
    let mut current: Option<LockedPackage> = None;
    let value = |line: &str, key: &str| -> Option<String> {
        line.strip_prefix(key)
            .and_then(|rest| rest.trim_start().strip_prefix('='))
            .and_then(toml_string_value)
    };
    for line in lock.lines().map(str::trim) {
        if line.starts_with('[') {
            packages.extend(current.take());
            if line == "[[package]]" {
                current = Some(LockedPackage {
                    name: String::new(),
                    version: String::new(),
                    source: None,
                });
            }
        } else if let Some(package) = current.as_mut() {
            if let Some(name) = value(line, "name") {
                package.name = name;
            } else if let Some(version) = value(line, "version") {
                package.version = version;
            } else if let Some(source) = value(line, "source") {
                package.source = Some(source);
            }
        }
    }
    packages.extend(current);
    packages
}

/// Whether the lock file `lock` has `name` at `version` as a dependency — from a
/// registry or git, not a package on disk — which is how the lock file of a
/// workspace that builds a registry crate lists it.
fn lock_has_dependency(lock: &str, name: &str, version: &str) -> bool {
    locked_packages(lock).iter().any(|package| {
        package.name == name && package.version == version && package.source.is_some()
    })
}

/// A `::name` dependency resolved to the package that provides it.
#[derive(Debug, PartialEq, Eq)]
struct OwnerPackage {
    /// The package, as `cargo build -p` takes it: its name, plus — when the
    /// dependency graph holds several versions of it — the version, as
    /// `name:version`.
    spec: String,
    /// The library crate the package builds (its name with `-` as `_`), which names
    /// its artifacts.
    crate_name: String,
    /// The version, if `spec` names one (the crate then has to be picked by it from
    /// what Cargo built).
    version: Option<String>,
}

/// Works out which package `::name` stands for.
///
/// * `declared` is what the owner's `Cargo.toml` says it is, if it declares a
///   dependency by that name: the `package = "…"` of a renamed one, else the key
///   itself. Otherwise `name` must be a package the lock file lists as a dependency
///   of the owner (up to `-` versus `_`).
/// * `locked` is the owner's dependency list from the lock file, if there is one. It
///   says which version of the package the owner resolves to. Where the lock file
///   holds several versions of the package, the spec carries that version, which is
///   what tells Cargo which of them to build. (A single one is left as the plain
///   name, as `-p name` is unambiguous then.) If the owner itself depends on several
///   (under different names: `dup = "2"`, `dup1 = { package = "dup", version = "1" }`),
///   the declared version picks among them.
///
/// The version is written `name:version`, which every Cargo understands. The
/// `name@version` form is the newer one, and Cargo 1.56 rejects it.
///
/// An `Err` is the end of a sentence beginning "subcrate `x` depends on `::name`, but".
fn owner_package(
    name: &str,
    declared: Option<&Declared>,
    locked: Option<&[LockedDep]>,
) -> Result<OwnerPackage, String> {
    let wanted = declared.map_or(name, |declared| declared.package.as_str());
    let mut found: Vec<&LockedDep> = Vec::new();
    if let Some(locked) = locked {
        // A package named exactly so, else one named so up to `-` versus `_`
        // (`::proc_macro2` is the package `proc-macro2`).
        found = locked.iter().filter(|dep| dep.name == wanted).collect();
        if found.is_empty() {
            found = locked
                .iter()
                .filter(|dep| dep.name.replace('-', "_") == wanted.replace('-', "_"))
                .collect();
        }
        found.dedup_by(|a, b| a.name == b.name && a.version == b.version);
    }
    let package = |dep: &LockedDep| {
        let version = if dep.copies > 1 {
            dep.version.clone()
        } else {
            None
        };
        OwnerPackage {
            spec: match &version {
                Some(version) => format!("{}:{}", dep.name, version),
                None => dep.name.clone(),
            },
            crate_name: dep.name.replace('-', "_"),
            version,
        }
    };
    match found.as_slice() {
        [] => match (declared, locked) {
            // Not a dependency of the owner according to the lock file, and the
            // manifest does not say otherwise.
            (None, Some(_)) => Err(format!("`{}` is not a dependency of this package", name)),
            // Nothing to go on but the name, which Cargo itself will check.
            _ => Ok(OwnerPackage {
                spec: wanted.to_string(),
                crate_name: wanted.replace('-', "_"),
                version: None,
            }),
        },
        [dep] => Ok(package(dep)),
        many => {
            // Several versions are among the owner's own dependencies: the one the
            // manifest's version (requirement) selects.
            let pin = declared.and_then(|declared| declared.pin.as_ref());
            let pinned: Vec<&&LockedDep> = many
                .iter()
                .filter(|dep| {
                    let version = dep.version.as_deref();
                    pin.map_or(false, |pin| version.map_or(false, |v| pin.matches(v)))
                })
                .collect();
            if let [dep] = pinned.as_slice() {
                return Ok(package(dep));
            }
            let versions: Vec<&str> = many
                .iter()
                .map(|dep| dep.version.as_deref().unwrap_or("?"))
                .collect();
            Err(format!(
                "this package depends on several versions of `{}` ({}), and `::{}` cannot be \
                 told to be one of them: give the dependency a `version` that only one satisfies",
                many[0].name,
                versions.join(", "),
                name
            ))
        }
    }
}

/// A cheap identity of a build artifact — its path, size and modification time —
/// that changes whenever Cargo rebuilds it.
fn file_stamp(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let modified = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |since| since.as_nanos());
            format!("{}:{}:{}", path.display(), meta.len(), modified)
        }
        Err(_) => format!("{}:missing", path.display()),
    }
}

fn rerun_if_changed(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());
}

fn warn(message: &str) {
    println!("cargo:warning={}", message);
}

/// Forwards a subcrate's `rustc` warnings to Cargo as `cargo:warning=` lines, so
/// they are shown like any other build warning. A header names the subcrate; the
/// locations in the lines point into its generated source, not the original file.
fn emit_subcrate_warnings(sub: &Subcrate, stderr: &str) {
    warn(&format!("warnings from subcrate `{}`:", sub.display_name()));
    for line in stderr.lines() {
        warn(line);
    }
}

/// Decodes the flags in `CARGO_ENCODED_RUSTFLAGS` (unit-separator delimited)
/// into individual arguments. Returns empty when unset or empty.
fn decode_rustflags() -> Vec<String> {
    match std::env::var("CARGO_ENCODED_RUSTFLAGS") {
        Ok(s) if !s.is_empty() => s.split('\u{1f}').map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

/// Produces `feature="<name>"` cfg strings for every feature of the owning
/// package that Cargo has enabled for this build.
///
/// Feature names are read from `Cargo.toml` (so dashes survive), and each is
/// matched against the `CARGO_FEATURE_<NAME>` variable Cargo sets for enabled
/// features — whose name mangling is lossy, which is why we start from the
/// manifest rather than enumerating the environment.
fn enabled_feature_cfgs(manifest_dir: &Path) -> Vec<String> {
    let text = match std::fs::read_to_string(manifest_dir.join("Cargo.toml")) {
        Ok(text) => text,
        Err(_) => return Vec::new(),
    };
    let mut cfgs = Vec::new();
    for name in feature_names(&text) {
        if std::env::var_os(feature_env_var(&name)).is_some() {
            cfgs.push(format!("feature=\"{}\"", name));
        }
    }
    cfgs
}

/// The `CARGO_FEATURE_<NAME>` variable Cargo sets for feature `name`
/// (uppercased, with every non-alphanumeric byte replaced by `_`).
fn feature_env_var(name: &str) -> String {
    let mut var = String::from("CARGO_FEATURE_");
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            var.push(ch.to_ascii_uppercase());
        } else {
            var.push('_');
        }
    }
    var
}

/// Collects the keys declared under `[features]` in a `Cargo.toml`.
fn feature_names(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_features = false;
    for raw in manifest.lines() {
        let line = strip_toml_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_features = line[1..line.len() - 1].trim() == "features";
            continue;
        }
        if !in_features {
            continue;
        }
        // A feature declaration is `name = [ … ]`; continuation lines of a
        // multi-line array have no `=` and are skipped.
        if let Some(eq) = line.find('=') {
            let key = line[..eq].trim().trim_matches('"').trim();
            if !key.is_empty() {
                names.push(key.to_string());
            }
        }
    }
    names
}

fn require_env(key: &str) -> Result<String, Error> {
    std::env::var(key).map_err(|_| {
        Error::new(format!(
            "`{}` is not set; `subcrate::build::compile()` must be called from a Cargo build script",
            key
        ))
    })
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("    {}", line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// An error produced by the build helper.
#[derive(Debug)]
pub struct Error {
    message: String,
}

impl Error {
    fn new(message: impl Into<String>) -> Self {
        Error {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::new(format!("subcrate build I/O error: {}", err))
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for dependency resolution and ordering. Dependency *cycles* and
    //! unresolvable dependencies are build errors, so they cannot be asserted from
    //! an end-to-end test; they are covered here instead.

    use super::*;

    // `compile()` and `Config::from_env()` must fail with a clear, actionable
    // error — not panic — when called outside a Cargo build script (`OUT_DIR`
    // unset). They live here, in the build-script-free `subcrate` crate: a crate
    // whose own build script set that environment could not observe its absence.
    #[test]
    fn compile_outside_a_build_script_reports_a_clear_error() {
        let error = compile().expect_err("compile() should fail without a build script");
        assert!(
            error.to_string().contains("build script"),
            "error should mention the build script, got: {}",
            error
        );
    }

    #[test]
    fn config_from_env_requires_build_script_env() {
        assert!(
            Config::from_env().is_err(),
            "Config::from_env() should fail without build-script environment"
        );
    }

    fn dep(alias: &str, path: &[&str]) -> Dependency {
        Dependency {
            alias: alias.to_string(),
            path: path.iter().map(|s| s.to_string()).collect(),
            external: false,
        }
    }

    /// Shorthand: a dependency on the subcrate named `name`, under that name.
    fn on(name: &str) -> Dependency {
        dep(name, &[name])
    }

    fn sub(root: &str, path: &[&str], deps: Vec<Dependency>) -> Subcrate {
        Subcrate {
            module_path: path.iter().map(|s| s.to_string()).collect(),
            root: PathBuf::from(root),
            source_file: PathBuf::from(root),
            inlined_files: false,
            deps,
            internal: format!("__subcrate_{}", path.join("_")),
            proc_macro: false,
            edition: None,
            inner_attrs: Vec::new(),
            items: Vec::new(),
            ui: UiExpectations::default(),
            gated: false,
        }
    }

    /// A top-level subcrate in `src/lib.rs`.
    fn top(name: &str, deps: Vec<Dependency>) -> Subcrate {
        sub("src/lib.rs", &[name], deps)
    }

    /// The package under test: `owner`, with a library at `src/lib.rs`.
    fn owner() -> Owner {
        Owner {
            name: "owner".to_string(),
            lib: Some(PathBuf::from("src/lib.rs")),
        }
    }

    /// A dependency on the external crate `name`, under that name.
    fn ext(name: &str) -> Dependency {
        Dependency {
            external: true,
            ..on(name)
        }
    }

    /// The compile order as module names, or the error message.
    fn order(subs: &[Subcrate]) -> Result<Vec<String>, String> {
        let links = resolve_links(subs, &owner()).map_err(|e| e.to_string())?;
        let needs = compile_deps(subs, &links, &owner());
        let order = compile_order(subs, &needs).map_err(|e| e.to_string())?;
        Ok(order.into_iter().map(|i| subs[i].display_name()).collect())
    }

    fn position(order: &[String], name: &str) -> usize {
        order.iter().position(|n| n == name).unwrap()
    }

    #[test]
    fn independent_subcrates_all_compile() {
        let order = order(&[top("a", vec![]), top("b", vec![])]).unwrap();
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn dependencies_compile_before_their_dependents() {
        // Declared against the grain: `c` uses `b`, which uses `a`.
        let subs = [
            top("c", vec![on("b")]),
            top("b", vec![on("a")]),
            top("a", vec![]),
        ];
        let order = order(&subs).unwrap();
        assert_eq!(order, ["a", "b", "c"]);
    }

    #[test]
    fn a_diamond_orders_the_shared_dependency_first() {
        let subs = [
            top("join", vec![on("left"), on("right")]),
            top("left", vec![on("base")]),
            top("right", vec![on("base")]),
            top("base", vec![]),
        ];
        let order = order(&subs).unwrap();
        assert_eq!(position(&order, "base"), 0);
        assert_eq!(position(&order, "join"), 3);
    }

    #[test]
    fn one_subcrate_under_two_aliases_is_a_single_edge() {
        let subs = [
            top("a", vec![]),
            top("b", vec![dep("x", &["a"]), dep("y", &["a"])]),
        ];
        assert_eq!(order(&subs).unwrap(), ["a", "b"]);
        let links = resolve_links(&subs, &owner()).unwrap();
        let aliases: Vec<&str> = links[1].iter().map(|l| l.alias.as_str()).collect();
        assert_eq!(aliases, ["x", "y"]);
    }

    #[test]
    fn a_dependency_cycle_is_an_error_naming_the_cycle() {
        let subs = [
            top("a", vec![on("b")]),
            top("b", vec![on("c")]),
            top("c", vec![on("a")]),
            top("unrelated", vec![]),
        ];
        let message = order(&subs).unwrap_err();
        let path = message
            .strip_prefix("subcrate dependency cycle: ")
            .unwrap_or_else(|| panic!("unexpected message: {}", message));
        let nodes: Vec<&str> = path.split(" -> ").collect();
        // The cycle closes on itself and visits exactly `a`, `b` and `c`.
        assert_eq!(nodes.first(), nodes.last());
        let mut distinct: Vec<&str> = nodes[1..].to_vec();
        distinct.sort_unstable();
        assert_eq!(distinct, ["a", "b", "c"]);
    }

    #[test]
    fn a_subcrate_depending_on_itself_is_a_cycle() {
        let message = order(&[top("a", vec![on("a")])]).unwrap_err();
        assert_eq!(message, "subcrate dependency cycle: a -> a");
    }

    #[test]
    fn an_unknown_dependency_is_an_error() {
        let message = order(&[top("a", vec![on("missing")])]).unwrap_err();
        assert!(message.contains("`a` depends on `missing`"), "{}", message);
        assert!(message.contains("no `#[subcrate]` module"), "{}", message);
        // It points at the syntax for a crate the package depends on.
        assert!(message.contains("write `::missing`"), "{}", message);
    }

    #[test]
    fn an_ident_matches_a_nested_subcrate_by_its_last_segment() {
        let subs = [
            top("b", vec![on("leaf")]),
            sub("src/lib.rs", &["nested", "leaf"], vec![]),
        ];
        assert_eq!(order(&subs).unwrap(), ["nested::leaf", "b"]);
    }

    #[test]
    fn a_full_path_picks_among_same_named_subcrates() {
        let subs = [
            sub("src/lib.rs", &["p", "h"], vec![]),
            sub("src/lib.rs", &["q", "h"], vec![]),
            top("a", vec![dep("h", &["q", "h"])]),
        ];
        let links = resolve_links(&subs, &owner()).unwrap();
        assert_eq!(links[2][0].target, LinkTarget::Subcrate(1));
        // The bare ident is ambiguous: two modules are called `h`.
        let subs = [
            sub("src/lib.rs", &["p", "h"], vec![]),
            sub("src/lib.rs", &["q", "h"], vec![]),
            top("a", vec![on("h")]),
        ];
        let message = order(&subs).unwrap_err();
        assert!(message.contains("ambiguous"), "{}", message);
        assert!(message.contains("`p::h`") && message.contains("`q::h`"));
    }

    #[test]
    fn an_exact_path_beats_a_longer_path_ending_the_same_way() {
        // `h` is both a top-level module and the tail of `p::h`; the top-level one
        // is what the path `h` names.
        let subs = [
            sub("src/lib.rs", &["p", "h"], vec![]),
            top("h", vec![]),
            top("a", vec![on("h")]),
        ];
        let links = resolve_links(&subs, &owner()).unwrap();
        assert_eq!(links[2][0].target, LinkTarget::Subcrate(1));
    }

    #[test]
    fn a_dependency_prefers_the_subcrate_under_its_own_crate_root() {
        // Two test files each define their own `helper`; each `user` gets its own.
        let subs = [
            sub("tests/x.rs", &["helper"], vec![]),
            sub("tests/y.rs", &["helper"], vec![]),
            sub("tests/y.rs", &["user"], vec![on("helper")]),
        ];
        let links = resolve_links(&subs, &owner()).unwrap();
        assert_eq!(links[2][0].target, LinkTarget::Subcrate(1));
    }

    #[test]
    fn a_dependency_may_come_from_another_crate_root() {
        let subs = [
            sub("src/lib.rs", &["shared"], vec![]),
            sub("tests/t.rs", &["user"], vec![on("shared")]),
        ];
        assert_eq!(order(&subs).unwrap(), ["shared", "user"]);
        // ...but `crate::<path>` only names it for the doc stand-in when the roots match.
        let links = resolve_links(&subs, &owner()).unwrap();
        assert!(doc_deps(&subs, 1, &links[1]).is_empty());
    }

    #[test]
    fn doc_deps_lists_same_root_dependencies_by_alias_and_full_path() {
        let subs = [
            sub("src/lib.rs", &["nested", "leaf"], vec![]),
            top("b", vec![dep("l", &["nested", "leaf"])]),
        ];
        let links = resolve_links(&subs, &owner()).unwrap();
        assert_eq!(
            doc_deps(&subs, 1, &links[1]),
            [(
                "l".to_string(),
                vec!["nested".to_string(), "leaf".to_string()]
            )]
        );
    }

    fn config() -> Config {
        Config {
            manifest_dir: PathBuf::from("."),
            out_dir: PathBuf::from("out"),
            rustc: PathBuf::from("rustc"),
            edition: "2021".to_string(),
            target: None,
            host: None,
            panic: None,
            opt_level: "0".to_string(),
            debug_assertions: true,
            rustflags: Vec::new(),
            feature_cfgs: Vec::new(),
            extra_rustc_args: Vec::new(),
            cargo: PathBuf::from("cargo"),
            package: "owner".to_string(),
            package_version: "0.1.0".to_string(),
            nested_workspace: std::cell::RefCell::new(None),
            profile: "debug".to_string(),
            nested: false,
            harvest_only: false,
            owner_deps_key: None,
            owner_deps: OwnerDeps::default(),
            host_deps: OwnerDeps::default(),
        }
    }

    fn linked(internal: &str, alias: &str) -> ExternDep {
        ExternDep {
            internal: internal.to_string(),
            alias: alias.to_string(),
            paths: vec![PathBuf::from(format!("out/subcrates/lib{}.rlib", internal))],
            owner_linked: false,
        }
    }

    #[test]
    fn a_dependency_is_linked_by_internal_name_and_imported_under_its_alias() {
        let sub = top("b", vec![]);
        let deps = [linked("__subcrate_a_1", "a"), linked("__subcrate_c_2", "z")];

        let args = config().rustc_args(&sub, &deps, Path::new("out/subcrates"));
        assert!(args.contains(&"--extern".to_string()));
        assert!(args.contains(&"__subcrate_a_1=out/subcrates/lib__subcrate_a_1.rlib".to_string()));
        assert!(args.contains(&"__subcrate_c_2=out/subcrates/lib__subcrate_c_2.rlib".to_string()));
        assert!(args.contains(&"dependency=out/subcrates".to_string()));

        let (source, _) = render_source(&[], &deps, &[], true, false);
        // `pub extern crate … as …;` (not `use`) so the alias lands in the extern
        // prelude: the subcrate can write `::a` and re-export it with `pub use a;`.
        assert!(
            source.contains("pub extern crate __subcrate_a_1 as a;"),
            "{}",
            source
        );
        assert!(
            source.contains("pub extern crate __subcrate_c_2 as z;"),
            "{}",
            source
        );
        assert!(!source.contains("use ::"), "{}", source);
    }

    #[test]
    fn a_proc_macro_subcrate_keeps_its_imports_private() {
        // A proc-macro crate may export only its macros, so a `pub extern crate`
        // there is a hard error; the import stays private (a proc-macro crate has
        // nothing to re-export anyway).
        let deps = [linked("__subcrate_a_1", "a")];
        let (source, _) = render_source(&[], &deps, &[], true, true);
        assert!(
            source.contains("extern crate __subcrate_a_1 as a;"),
            "{}",
            source
        );
        assert!(!source.contains("pub extern crate"), "{}", source);
    }

    #[test]
    fn a_crate_under_two_aliases_is_linked_once() {
        let deps = [linked("__subcrate_a_1", "x"), linked("__subcrate_a_1", "y")];
        let args = config().rustc_args(&top("b", vec![]), &deps, Path::new("out/subcrates"));
        assert_eq!(args.iter().filter(|a| *a == "--extern").count(), 1);
        let (source, _) = render_source(&[], &deps, &[], true, false);
        assert!(source.contains("pub extern crate __subcrate_a_1 as x;"));
        assert!(source.contains("pub extern crate __subcrate_a_1 as y;"));
    }

    #[test]
    fn the_injected_import_follows_crate_level_attributes() {
        let file = syn::parse_file("#![allow(dead_code)] fn f() {}").unwrap();
        let (source, _) = render_source(
            &file.attrs,
            &[linked("__subcrate_a_1", "a")],
            &file.items,
            true,
            false,
        );
        let attr = source.find("allow").unwrap();
        let import = source.find("pub extern crate __subcrate_a_1").unwrap();
        let item = source.find("fn f").unwrap();
        assert!(attr < import && import < item, "{}", source);
    }

    #[test]
    fn the_rendered_source_is_multi_line_and_re_parses() {
        // The body is pretty-printed for readable diagnostic locations; the result
        // must stay valid Rust.
        let file =
            syn::parse_file("pub fn f() -> usize { let x = 1; x + 2 }\npub struct S { a: u32 }")
                .unwrap();
        let (source, _) = render_source(&[], &[], &file.items, true, false);
        assert!(
            source.lines().count() > 3,
            "expected multi-line source, got:\n{}",
            source
        );
        let back = syn::parse_file(&source).expect("rendered source must re-parse");
        assert_eq!(back.items.len(), 2);
    }

    #[test]
    fn a_subcrate_without_dependencies_renders_no_import() {
        let args = config().rustc_args(&top("b", vec![]), &[], Path::new("out/subcrates"));
        assert!(!args.contains(&"--extern".to_string()));
        assert!(!args.contains(&"-L".to_string()));
        assert!(!render_source(&[], &[], &[], true, false)
            .0
            .contains("extern crate"));
    }

    // -- Owner dependencies: crates the package depends on, and the owner crate ----

    #[test]
    fn an_external_dependency_resolves_to_an_external_crate() {
        let subs = [top("a", vec![ext("serde")])];
        let links = resolve_links(&subs, &owner()).unwrap();
        assert_eq!(links[0][0].alias, "serde");
        assert_eq!(
            links[0][0].target,
            LinkTarget::External("serde".to_string())
        );
        // It is built by Cargo, not by us, so it takes no part in the ordering.
        assert_eq!(order(&subs).unwrap(), ["a"]);
    }

    #[test]
    fn an_external_dependency_can_be_renamed() {
        let mut renamed = ext("serde");
        renamed.alias = "s".to_string();
        let links = resolve_links(&[top("a", vec![renamed])], &owner()).unwrap();
        assert_eq!(links[0][0].alias, "s");
        assert_eq!(
            links[0][0].target,
            LinkTarget::External("serde".to_string())
        );
    }

    #[test]
    fn an_external_dependency_names_a_crate_not_a_path_into_it() {
        let mut path = ext("serde");
        path.path.push("de".to_string());
        let message = order(&[top("a", vec![path])]).unwrap_err();
        assert!(message.contains("`::serde::de`"), "{}", message);
        assert!(message.contains("like `::serde`"), "{}", message);
    }

    #[test]
    fn an_external_dependency_and_a_subcrate_cannot_share_a_name() {
        let subs = [
            top("serde", vec![]),
            top("a", vec![on("serde"), ext("serde")]),
        ];
        let message = order(&subs).unwrap_err();
        assert!(message.contains("more than one dependency named `serde`"));
    }

    #[test]
    fn the_owner_crate_is_a_dependency_outside_the_library() {
        let user = sub("tests/t.rs", &["user"], vec![ext("owner")]);
        let links = resolve_links(&[user], &owner()).unwrap();
        assert_eq!(links[0][0].alias, "owner");
        assert_eq!(links[0][0].target, LinkTarget::Owner);
    }

    #[test]
    fn the_owner_crate_may_be_used_from_binaries_examples_and_benches() {
        for root in [
            "src/main.rs",
            "src/bin/tool.rs",
            "examples/e.rs",
            "benches/b.rs",
        ] {
            let user = sub(root, &["user"], vec![ext("owner")]);
            assert!(resolve_links(&[user], &owner()).is_ok(), "{}", root);
        }
    }

    #[test]
    fn a_subcrate_in_the_library_cannot_use_the_owner_crate() {
        let message = order(&[top("a", vec![ext("owner")])]).unwrap_err();
        assert!(message.contains("subcrate `a`"), "{}", message);
        assert!(message.contains("dependency cycle"), "{}", message);
        assert!(message.contains("`::owner`"), "{}", message);
    }

    #[test]
    fn a_package_without_a_library_has_no_owner_crate_to_use() {
        let no_lib = Owner {
            lib: None,
            ..owner()
        };
        let user = sub("src/main.rs", &["user"], vec![ext("owner")]);
        let message = resolve_links(&[user], &no_lib).unwrap_err().to_string();
        assert!(message.contains("has no library"), "{}", message);
    }

    #[test]
    fn the_owner_crate_is_the_packages_library_named_with_underscores() {
        let dir = std::env::temp_dir().join(format!("subcrate-owner-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let mut config = config();
        config.package = "my-package".to_string();
        config.manifest_dir = dir.clone();

        // No `src/lib.rs`: no library.
        let without = config.owner();
        std::fs::write(dir.join("src").join("lib.rs"), "").unwrap();
        let with = config.owner();
        // `owner()` returns the canonical path; resolve the expectation the same way
        // while the file still exists (temp dirs are symlinked on some platforms).
        let expected_lib = std::fs::canonicalize(dir.join("src").join("lib.rs")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(without.name, "my_package");
        assert_eq!(without.lib, None);
        assert_eq!(with.name, "my_package");
        assert_eq!(with.lib, Some(expected_lib));
    }

    #[test]
    fn a_library_subcrate_matches_the_owner_despite_an_odd_lib_path() {
        // A `[lib] path` can spell the library file differently from where discovery
        // scans it (here via `..`; a symlinked directory does the same). Both the
        // owner's library and the discovered crate root are canonicalized, so the
        // library subcrate is still recognized as the library's — the comparison
        // `needed_by_lib`/`compile_deps` rely on — rather than mistaken for a test's.
        let dir = std::env::temp_dir().join(format!("subcrate-canon-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"p\"\n\n[lib]\npath = \"src/../src/lib.rs\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src").join("lib.rs"),
            "#[subcrate] mod libsub {}\n",
        )
        .unwrap();

        let discovered = discover(&dir);
        let mut config = config();
        config.manifest_dir = dir.clone();
        let owner = config.owner();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(discovered.subcrates.len(), 1);
        assert_eq!(
            owner.lib.as_deref(),
            Some(discovered.subcrates[0].root.as_path())
        );
    }

    #[test]
    fn the_owner_crate_is_built_after_the_subcrates_of_its_library() {
        // `user` is declared first, but it links the library, which is made of `lib_a`
        // and `lib_b` (and not of `other`, which is in another crate root).
        let subs = [
            sub("tests/t.rs", &["user"], vec![ext("owner")]),
            top("lib_a", vec![]),
            top("lib_b", vec![]),
            sub("tests/t.rs", &["other"], vec![]),
        ];
        let order = order(&subs).unwrap();
        assert!(position(&order, "lib_a") < position(&order, "user"));
        assert!(position(&order, "lib_b") < position(&order, "user"));
    }

    #[test]
    fn a_library_subcrate_depending_on_one_that_uses_the_owner_crate_is_a_cycle() {
        // The library would need `user`, which needs the library.
        let subs = [
            top("lib_a", vec![on("user")]),
            sub("tests/t.rs", &["user"], vec![ext("owner")]),
        ];
        let message = order(&subs).unwrap_err();
        assert!(message.contains("subcrate dependency cycle"), "{}", message);
    }

    #[test]
    fn a_nested_build_needs_only_the_subcrates_the_library_uses() {
        let subs = [
            top("lib_a", vec![on("shared")]),
            sub("tests/t.rs", &["shared"], vec![]),
            sub("tests/t.rs", &["user"], vec![ext("owner")]),
            sub("tests/t.rs", &["unrelated"], vec![]),
        ];
        let links = resolve_links(&subs, &owner()).unwrap();
        let needs = compile_deps(&subs, &links, &owner());
        // The library itself, and what it depends on: not the owner crate's users.
        assert_eq!(
            needed_by_lib(&subs, &needs, &owner()),
            [true, true, false, false]
        );
    }

    #[test]
    fn a_subcrate_is_owner_linked_directly_or_through_a_sibling() {
        let subs = [
            top("above", vec![on("mid")]),
            top("mid", vec![ext("serde")]),
            top("plain", vec![]),
        ];
        let links = resolve_links(&subs, &owner()).unwrap();
        let needs = compile_deps(&subs, &links, &owner());
        let order = compile_order(&subs, &needs).unwrap();
        assert_eq!(owner_linked(&links, &order), [true, true, false]);
    }

    fn external_artifacts() -> OwnerDeps {
        let mut deps = OwnerDeps::default();
        deps.crates.insert(
            "leaf".to_string(),
            OwnerCrate {
                rlib: Some(PathBuf::from("t/debug/libleaf.rlib")),
                rmeta: Some(PathBuf::from("t/debug/build/leaf/h/out/libleaf-h.rmeta")),
                ..OwnerCrate::default()
            },
        );
        deps.lib_dirs.insert(PathBuf::from("t/debug"));
        deps.lib_dirs
            .insert(PathBuf::from("t/debug/build/leaf/h/out"));
        deps
    }

    #[test]
    fn an_external_dependency_is_linked_with_its_rlib_and_its_rmeta() {
        let mut config = config();
        config.owner_deps = external_artifacts();
        let sub = top("a", vec![ext("leaf")]);
        let links = [Link {
            alias: "l".to_string(),
            target: LinkTarget::External("leaf".to_string()),
        }];
        let deps = config
            .extern_deps(&[sub], 0, &links, Path::new("out/subcrates"), &[true])
            .unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].internal, "leaf");
        assert_eq!(deps[0].alias, "l");
        assert!(deps[0].owner_linked);

        let sub = top("a", vec![]);
        let args = config.rustc_args(&sub, &deps, Path::new("out/subcrates"));
        // Both files, under the one crate name (the rlib alone is a metadata stub).
        assert!(args.contains(&"leaf=t/debug/libleaf.rlib".to_string()));
        assert!(args.contains(&"leaf=t/debug/build/leaf/h/out/libleaf-h.rmeta".to_string()));
        // The directories of the crate and its own dependencies are searched.
        assert!(args.contains(&"dependency=t/debug".to_string()));
        assert!(args.contains(&"dependency=t/debug/build/leaf/h/out".to_string()));

        let (source, _) = render_source(&[], &deps, &[], true, false);
        assert!(source.contains("pub extern crate leaf as l;"), "{}", source);
    }

    #[test]
    fn only_subcrates_built_on_owner_dependencies_search_their_directories() {
        let mut config = config();
        config.owner_deps = external_artifacts();
        let args = config.rustc_args(
            &top("a", vec![]),
            &[linked("__subcrate_b_1", "b")],
            Path::new("out/subcrates"),
        );
        assert!(!args.contains(&"dependency=t/debug".to_string()));
    }

    #[test]
    fn an_owner_dependency_that_was_not_built_is_an_error() {
        let config = config();
        let sub = top("a", vec![ext("leaf")]);
        let links = [Link {
            alias: "leaf".to_string(),
            target: LinkTarget::External("leaf".to_string()),
        }];
        let result = config.extern_deps(&[sub], 0, &links, Path::new("out"), &[true]);
        let message = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(message.contains("`::leaf`"), "{}", message);
        assert!(message.contains("no library artifact"), "{}", message);
    }

    fn cross_config() -> Config {
        let mut config = config();
        config.target = Some("aarch64-unknown-linux-gnu".to_string());
        config.host = Some("x86_64-unknown-linux-gnu".to_string());
        config
    }

    #[test]
    fn a_proc_macro_subcrate_links_the_hosts_copy_of_an_owner_dependency_when_cross_compiling() {
        // The target's build holds `leaf` at `target/…`, the host's at `host/…`: a
        // proc-macro subcrate (which runs on the host) takes the host's, a library
        // subcrate the target's.
        let mut config = cross_config();
        config.owner_deps = external_artifacts();
        let mut host = external_artifacts();
        for krate in host.crates.values_mut() {
            krate.rlib = krate
                .rlib
                .take()
                .map(|rlib| PathBuf::from("host").join(rlib));
            krate.rmeta = krate
                .rmeta
                .take()
                .map(|rmeta| PathBuf::from("host").join(rmeta));
        }
        host.lib_dirs = [PathBuf::from("host/deps")].into_iter().collect();
        config.host_deps = host;
        let links = [Link {
            alias: "leaf".to_string(),
            target: LinkTarget::External("leaf".to_string()),
        }];
        let mut proc_macro = top("a", vec![ext("leaf")]);
        proc_macro.proc_macro = true;
        let library = top("a", vec![ext("leaf")]);
        let paths = |sub: Subcrate| -> Vec<PathBuf> {
            let deps = config
                .extern_deps(&[sub], 0, &links, Path::new("out"), &[true])
                .unwrap();
            deps.into_iter().flat_map(|dep| dep.paths).collect()
        };
        let host_paths = paths(proc_macro);
        assert!(!host_paths.is_empty());
        assert!(
            host_paths.iter().all(|path| path.starts_with("host")),
            "{:?}",
            host_paths
        );
        let target_paths = paths(library);
        assert!(!target_paths.is_empty());
        assert!(
            target_paths.iter().all(|path| !path.starts_with("host")),
            "{:?}",
            target_paths
        );
        // Both builds' directories are searched by a subcrate built on either.
        let dirs: Vec<&PathBuf> = config.dependency_dirs().collect();
        assert!(dirs.contains(&&PathBuf::from("host/deps")), "{:?}", dirs);
    }

    #[test]
    fn a_proc_macro_subcrate_cannot_use_the_owner_crate_when_cross_compiling() {
        let config = cross_config();
        let mut sub = sub("tests/t.rs", &["user"], vec![ext("owner")]);
        sub.proc_macro = true;
        let subs = [sub];
        let links = resolve_links(&subs, &owner()).unwrap();
        // Refused before anything is built (a `cargo` that cannot be launched proves
        // that), and by the link itself.
        let mut config = config;
        config.cargo = PathBuf::from("/nonexistent/cargo");
        let message = config
            .build_host_deps(&subs, &links, &[true])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(message.contains("cross-compiling"), "{}", message);
        assert!(message.contains("owner crate `::owner`"), "{}", message);
        let message = config
            .extern_deps(&subs, 0, &links[0], Path::new("out"), &[true])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(message.contains("cross-compiling"), "{}", message);
    }

    #[test]
    fn a_library_subcrate_of_a_cross_compiled_package_does_not_need_the_host_build() {
        // Nothing is built for the host unless a proc-macro subcrate uses a crate.
        let mut config = cross_config();
        config.cargo = PathBuf::from("/nonexistent/cargo");
        let subs = [top("a", vec![ext("leaf")])];
        let links = resolve_links(&subs, &owner()).unwrap();
        let deps = config.build_host_deps(&subs, &links, &[true]).unwrap();
        assert!(deps.crates.is_empty() && deps.lib_dirs.is_empty());
        // And not when the package is not cross-compiled at all: the proc-macro
        // subcrate takes the one build.
        let mut native = config;
        native.target = None;
        native.host = None;
        let mut proc_macro = top("a", vec![ext("leaf")]);
        proc_macro.proc_macro = true;
        let subs = [proc_macro];
        let links = resolve_links(&subs, &owner()).unwrap();
        let deps = native.build_host_deps(&subs, &links, &[true]).unwrap();
        assert!(deps.crates.is_empty());
    }

    #[test]
    fn nothing_is_built_when_no_subcrate_uses_an_owner_dependency() {
        let subs = [top("a", vec![]), top("b", vec![on("a")])];
        let links = resolve_links(&subs, &owner()).unwrap();
        // (A `cargo` that cannot be launched proves no process is started.)
        let mut config = config();
        config.cargo = PathBuf::from("/nonexistent/cargo");
        let deps = config
            .build_owner_deps(&subs, &links, &[true, true])
            .unwrap();
        assert!(deps.crates.is_empty() && deps.lib_dirs.is_empty());
    }

    #[test]
    fn a_dependency_missing_from_the_lock_file_is_reported_before_building() {
        let dir = std::env::temp_dir().join(format!("subcrate-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.lock"), LOCK).unwrap();
        let subs = [top("a", vec![ext("notadep")])];
        let links = resolve_links(&subs, &owner()).unwrap();
        let mut config = config();
        config.manifest_dir = dir.clone();
        config.cargo = PathBuf::from("/nonexistent/cargo");
        let result = config.build_owner_deps(&subs, &links, &[true]);
        let _ = std::fs::remove_dir_all(&dir);
        let message = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(message.contains("subcrate `a`"), "{}", message);
        assert!(
            message.contains("`notadep` is not a dependency of this package"),
            "{}",
            message
        );
    }

    #[test]
    fn a_nested_build_never_builds_the_owner_crate() {
        // The backstop: if a subcrate using the owner crate reached a nested build,
        // building it would run this build script again, forever.
        let user = sub("tests/t.rs", &["user"], vec![ext("owner")]);
        let subs = [user];
        let links = resolve_links(&subs, &owner()).unwrap();
        let mut config = config();
        config.nested = true;
        config.cargo = PathBuf::from("/nonexistent/cargo");
        let message = config
            .build_owner_deps(&subs, &links, &[true])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(message.contains("dependency cycle"), "{}", message);
        // ...though the same subcrate is simply not wanted in a nested build.
        let needs = compile_deps(&subs, &links, &owner());
        let wanted = needed_by_lib(&subs, &needs, &owner());
        assert!(config.build_owner_deps(&subs, &links, &wanted).is_ok());
    }

    /// Runs `Config::build_owner_deps` for `subs` (all wanted) with a stand-in for
    /// Cargo — `/bin/sh build …` run in the package's directory, where `build` is a
    /// script that records its arguments and answers with `messages` — and returns
    /// what it built and the arguments it was given, followed by three more lines:
    /// `SUBCRATE_NESTED=…`, `SUBCRATE_HARVEST_ONLY=…` (`unset`, or the value) and
    /// `PWD=…`, as the nested build saw them. The package is `owner`, with the
    /// given manifest and lock file and its build directory `target/debug/build`.
    #[cfg(unix)]
    fn nested_build(
        test: &str,
        manifest: &str,
        lock: &str,
        messages: &str,
        subs: &[Subcrate],
    ) -> (Result<OwnerDeps, Error>, Vec<String>, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("subcrate-nested-{}-{}", test, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), manifest).unwrap();
        std::fs::write(dir.join("Cargo.lock"), lock).unwrap();
        std::fs::write(dir.join("messages.txt"), messages).unwrap();
        // Records the arguments, and then the environment that matters to a nested
        // `subcrate` build script (`NAME=value`, `unset` when it is not set, and the
        // physical directory the build runs in).
        std::fs::write(
            dir.join("build"),
            "printf '%s\\n' \"$@\" \"SUBCRATE_NESTED=${SUBCRATE_NESTED-unset}\" \
             \"SUBCRATE_HARVEST_ONLY=${SUBCRATE_HARVEST_ONLY-unset}\" \"PWD=$(pwd -P)\" > args.txt\n\
             cat messages.txt\n",
        )
        .unwrap();
        let mut config = config();
        config.cargo = PathBuf::from("/bin/sh");
        config.manifest_dir = dir.clone();
        config.out_dir = dir.join("target/debug/build/owner-1a/out");
        let links = resolve_links(subs, &owner()).unwrap();
        let result = config.build_owner_deps(subs, &links, &vec![true; subs.len()]);
        let args = std::fs::read_to_string(dir.join("args.txt"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        (result, args, dir)
    }

    /// The packages asked for in the arguments of a nested build (`-p <spec>`).
    #[cfg(unix)]
    fn packages_asked_for(args: &[String]) -> Vec<&str> {
        args.windows(2)
            .filter(|pair| pair[0] == "-p")
            .map(|pair| pair[1].as_str())
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn the_nested_build_asks_for_the_package_a_name_stands_for() {
        let manifest = "[package]\nname = \"owner\"\n\n[dependencies]\n\
                        dup = \"2\"\nmyleaf = { package = \"leaf\", path = \"../leaf\" }\n\
                        mid = { path = \"../mid\" }\n";
        let messages = format!(
            "{}\n{}\n",
            dup_message("1.0.0", true),
            dup_message("2.0.0", true)
        );
        // `::dup` has two versions in the graph, `::myleaf` is the package `leaf`,
        // and `::leaf` (not what the package calls it) finds the same package again.
        let subs = [top("a", vec![ext("dup"), ext("myleaf"), ext("leaf")])];
        let (result, args, dir) = nested_build("rename", manifest, MULTI_LOCK, &messages, &subs);
        let deps = result.unwrap();
        // The owner joins the selection because `myleaf`/`mid` are path dependencies
        // (so possibly workspace members); the resolved dependency specs are the point
        // here.
        assert_eq!(packages_asked_for(&args), ["dup:2.0.0", "leaf", "owner"]);
        assert_eq!(args.iter().filter(|arg| *arg == "-p").count(), 3);

        // Each name is tied to its crate, and the version to pick.
        let external = |name: &str| {
            (
                deps.externals[name].crate_name.clone(),
                deps.externals[name].version.clone(),
            )
        };
        assert_eq!(
            external("dup"),
            ("dup".to_string(), Some("2.0.0".to_string()))
        );
        assert_eq!(external("myleaf"), ("leaf".to_string(), None));
        assert_eq!(external("leaf"), ("leaf".to_string(), None));

        // The nested build is in the profile directory, not in `OUT_DIR`.
        let at = args.iter().position(|arg| arg == "--target-dir").unwrap();
        let target_dir = PathBuf::from(&args[at + 1]);
        assert!(
            target_dir.starts_with(dir.join("target/debug/subcrate-owner-deps/owner")),
            "{}",
            target_dir.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_nested_build_includes_the_owner_for_the_owner_crate_or_a_path_dependency() {
        // A registry dependency (`leaf = "1"`) is never a workspace member, so the
        // owner is left out — unless a subcrate also uses the owner crate itself.
        let registry = "[package]\nname = \"owner\"\n\n[dependencies]\nleaf = \"1\"\n";
        let leaf = |is_owner: bool| {
            if is_owner {
                sub("tests/t.rs", &["user"], vec![ext("leaf"), ext("owner")])
            } else {
                sub("tests/t.rs", &["user"], vec![ext("leaf")])
            }
        };
        let (result, args, _) = nested_build("no-owner", registry, MULTI_LOCK, "", &[leaf(false)]);
        result.unwrap();
        assert_eq!(packages_asked_for(&args), ["leaf"]);

        let (result, args, _) = nested_build("owner", registry, MULTI_LOCK, "", &[leaf(true)]);
        result.unwrap();
        assert_eq!(packages_asked_for(&args), ["leaf", "owner"]);

        // A path dependency may be a workspace member, so the owner joins the
        // selection to unify its features with the main build's (see
        // `build_owner_deps`).
        let path = "[package]\nname = \"owner\"\n\n[dependencies]\n\
                    leaf = { path = \"../leaf\" }\n";
        let (result, args, _) = nested_build("path", path, MULTI_LOCK, "", &[leaf(false)]);
        result.unwrap();
        assert_eq!(packages_asked_for(&args), ["leaf", "owner"]);
    }

    #[test]
    fn the_nested_cargo_build_is_into_a_separate_target_directory() {
        let args_for = |config: &Config, host: bool| -> Vec<String> {
            config
                .cargo_args(
                    &["serde".to_string(), "owner".to_string()],
                    Path::new("out/owner-deps-target"),
                    host,
                )
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };
        let args = |config: &Config| args_for(config, false);

        let debug = args(&config());
        assert_eq!(debug[0], "build");
        assert_eq!(&debug[1..5], ["-p", "serde", "-p", "owner"]);
        for flag in [
            "--lib",
            "--offline",
            "--message-format=json-render-diagnostics",
        ] {
            assert!(debug.contains(&flag.to_string()), "{:?}", debug);
        }
        let at = debug.iter().position(|a| a == "--target-dir").unwrap();
        assert_eq!(debug[at + 1], "out/owner-deps-target");
        assert!(!debug.contains(&"--release".to_string()));
        assert!(!debug.contains(&"--target".to_string()));

        let mut release = config();
        release.profile = "release".to_string();
        assert!(args(&release).contains(&"--release".to_string()));

        let mut cross = config();
        cross.target = Some("aarch64-unknown-linux-gnu".to_string());
        cross.host = Some("x86_64-unknown-linux-gnu".to_string());
        let target_args = args(&cross);
        let at = target_args.iter().position(|a| a == "--target").unwrap();
        assert_eq!(target_args[at + 1], "aarch64-unknown-linux-gnu");
        // The build for a proc-macro subcrate's crates names the host explicitly.
        let host_args = args_for(&cross, true);
        let at = host_args.iter().position(|a| a == "--target").unwrap();
        assert_eq!(host_args[at + 1], "x86_64-unknown-linux-gnu");
    }

    /// Runs `build_host_deps` for `subs` (all wanted) on a cross-compiling `config` with
    /// the stand-in for Cargo of [`nested_build`], returning what it built, the
    /// arguments it was given (followed by the three environment lines, as there) and
    /// its own directory (removed).
    #[cfg(unix)]
    fn nested_host_build(
        test: &str,
        manifest: &str,
        lock: &str,
        messages: &str,
        subs: &[Subcrate],
    ) -> (Result<OwnerDeps, Error>, Vec<String>, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("subcrate-host-{}-{}", test, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), manifest).unwrap();
        std::fs::write(dir.join("Cargo.lock"), lock).unwrap();
        std::fs::write(dir.join("messages.txt"), messages).unwrap();
        std::fs::write(
            dir.join("build"),
            "printf '%s\\n' \"$@\" \"SUBCRATE_NESTED=${SUBCRATE_NESTED-unset}\" \
             \"SUBCRATE_HARVEST_ONLY=${SUBCRATE_HARVEST_ONLY-unset}\" \"PWD=$(pwd -P)\" > args.txt\n\
             cat messages.txt\n",
        )
        .unwrap();
        let mut config = cross_config();
        config.cargo = PathBuf::from("/bin/sh");
        config.manifest_dir = dir.clone();
        config.out_dir = dir.join("target/aarch64-unknown-linux-gnu/debug/build/owner-1a/out");
        let links = resolve_links(subs, &owner()).unwrap();
        let result = config.build_host_deps(subs, &links, &vec![true; subs.len()]);
        let args = std::fs::read_to_string(dir.join("args.txt"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        (result, args, dir)
    }

    #[cfg(unix)]
    #[test]
    fn the_crates_of_a_proc_macro_subcrate_are_built_for_the_host_when_cross_compiling() {
        let manifest = "[package]\nname = \"owner\"\n\n[dependencies]\ndup = \"2\"\n";
        let mut proc_macro = top("a", vec![ext("dup")]);
        proc_macro.proc_macro = true;
        let message = dup_message("2.0.0", true);
        let (result, args, dir) =
            nested_host_build("pm", manifest, MULTI_LOCK, &message, &[proc_macro]);
        let deps = result.unwrap();
        assert_eq!(packages_asked_for(&args), ["dup:2.0.0"]);
        // For the host, by its triple, in a directory of its own.
        let at = args.iter().position(|arg| arg == "--target").unwrap();
        assert_eq!(args[at + 1], "x86_64-unknown-linux-gnu");
        let at = args.iter().position(|arg| arg == "--target-dir").unwrap();
        let target_dir = PathBuf::from(&args[at + 1]);
        assert!(
            target_dir.starts_with(
                dir.join("target/aarch64-unknown-linux-gnu/debug/subcrate-owner-deps/owner")
            ),
            "{}",
            target_dir.display()
        );
        assert!(target_dir.to_string_lossy().ends_with("-host") || target_dir.ends_with("host"));
        assert_eq!(deps.externals["dup"].crate_name, "dup");
        assert!(deps.crates.contains_key("dup"));

        // A library subcrate of the same package is not its business.
        let library = top("a", vec![ext("dup")]);
        let (result, args, _) =
            nested_host_build("lib", manifest, MULTI_LOCK, &message, &[library]);
        assert!(result.unwrap().crates.is_empty());
        assert!(args.is_empty(), "{:?}", args);
    }

    #[cfg(unix)]
    #[test]
    fn a_path_dependency_selects_the_owner_in_the_host_build_too() {
        // As in the target's build (see `build_owner_deps`): Cargo only finds a
        // registry crate next to a workspace member if the owner is selected.
        let manifest = "[package]\nname = \"owner\"\n\n[dependencies]\n\
                        leaf = { path = \"../leaf\" }\ndup = \"2\"\n";
        let mut proc_macro = top("a", vec![ext("leaf"), ext("dup")]);
        proc_macro.proc_macro = true;
        let message = dup_message("2.0.0", true);
        let (result, args, _) =
            nested_host_build("path", manifest, MULTI_LOCK, &message, &[proc_macro]);
        result.unwrap();
        assert_eq!(packages_asked_for(&args), ["dup:2.0.0", "leaf", "owner"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_nested_build_that_panics_in_cargos_resolver_says_what_the_dependency_needs() {
        let dir = std::env::temp_dir().join(format!("subcrate-panic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"owner\"\n\n[dependencies]\nleaf = \"1\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("Cargo.lock"), MULTI_LOCK).unwrap();
        std::fs::write(
            dir.join("build"),
            "echo \"thread 'main' panicked at src/cargo/core/resolver/features.rs:325:13:\" >&2\nexit 101\n",
        )
        .unwrap();
        let mut config = config();
        config.cargo = PathBuf::from("/bin/sh");
        config.manifest_dir = dir.clone();
        config.out_dir = dir.join("target/debug/build/owner-1a/out");
        let subs = [top("a", vec![ext("leaf")])];
        let links = resolve_links(&subs, &owner()).unwrap();
        let result = config.build_owner_deps(&subs, &links, &[true]);
        let _ = std::fs::remove_dir_all(&dir);
        let message = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            message.contains("failed to build the crates"),
            "{}",
            message
        );
        assert!(message.contains("normal dependency"), "{}", message);
        assert!(message.contains("optional"), "{}", message);
    }

    fn temp_dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("subcrate-{}-{}", test, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_packaged_directory_is_told_by_its_original_manifest_or_by_where_cargo_puts_it() {
        let dir = temp_dir("packaged-dir");
        assert!(!is_packaged_dir(&dir));
        std::fs::write(dir.join("Cargo.toml.orig"), "").unwrap();
        assert!(is_packaged_dir(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        for packaged in [
            "/home/u/.cargo/registry/src/index.crates.io-6f17d22bba15001f/serde-1.0.0",
            "/home/u/.cargo/git/checkouts/repo-1a2b3c/abcdef0",
        ] {
            assert!(is_packaged_dir(Path::new(packaged)), "{}", packaged);
        }
        for source in [
            "/home/u/src/serde",
            "/home/u/registry/serde",
            "/home/u/git/checkout",
        ] {
            assert!(!is_packaged_dir(Path::new(source)), "{}", source);
        }
    }

    #[test]
    fn a_manifest_that_belongs_to_a_workspace_is_told() {
        assert!(mentions_workspace(
            "[package]\nname = \"a\"\n[workspace]\nmembers = []\n"
        ));
        assert!(mentions_workspace("[package]\nversion.workspace = true\n"));
        assert!(mentions_workspace(
            "[dependencies]\nserde = { workspace = true }\n"
        ));
        assert!(mentions_workspace("[workspace.dependencies]\nx = \"1\"\n"));
        assert!(!mentions_workspace(
            "[package]\nname = \"a\"\n# the workspace = nothing\n[dependencies]\nx = \"1\"\n"
        ));
    }

    #[test]
    fn the_lock_file_of_a_registry_dependency_is_told_from_one_that_lists_the_package_on_disk() {
        let lock = "version = 3

[[package]]
name = \"a\"
version = \"0.1.0\"

[[package]]
name = \"b\"
version = \"1.2.3\"
source = \"registry+https://github.com/rust-lang/crates.io-index\"
checksum = \"00\"

[[package]]
name = \"b\"
version = \"2.0.0\"
";
        let packages = locked_packages(lock);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[1].name, "b");
        assert_eq!(packages[1].version, "1.2.3");
        assert!(packages[1]
            .source
            .as_deref()
            .unwrap()
            .starts_with("registry+"));
        assert_eq!(packages[2].source, None);
        assert!(lock_has_dependency(lock, "b", "1.2.3"));
        // A package on disk is the workspace's own, not a dependency; and a version
        // that is not locked is not it either.
        assert!(!lock_has_dependency(lock, "a", "0.1.0"));
        assert!(!lock_has_dependency(lock, "b", "2.0.0"));
        assert!(!lock_has_dependency(lock, "b", "1.2.4"));
        assert!(!lock_has_dependency(lock, "c", "1.2.3"));
    }

    const REGISTRY_LOCK: &str = "version = 3

[[package]]
name = \"owner\"
version = \"0.1.0\"
source = \"registry+https://github.com/rust-lang/crates.io-index\"
checksum = \"00\"
dependencies = [
 \"leaf\",
]

[[package]]
name = \"leaf\"
version = \"1.0.0\"
source = \"registry+https://github.com/rust-lang/crates.io-index\"
checksum = \"11\"
";

    /// A packaged `owner` (the marker `Cargo.toml.orig`, its own lock file) that
    /// `dir/ws`'s build — `dir/ws/target/debug/build/owner-1a/out` — has as a dependency.
    fn packaged_owner(dir: &Path, manifest: &str, own_lock: &str, ws_lock: &str) -> Config {
        let package = dir.join("registry/owner-0.1.0");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("Cargo.toml"), manifest).unwrap();
        std::fs::write(package.join("Cargo.toml.orig"), manifest).unwrap();
        std::fs::write(package.join("Cargo.lock"), own_lock).unwrap();
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("Cargo.lock"), ws_lock).unwrap();
        let mut config = config();
        config.manifest_dir = package;
        config.out_dir = ws.join("target/debug/build/owner-1a/out");
        config
    }

    #[test]
    fn a_packaged_crate_is_built_against_the_versions_of_the_workspace_that_builds_it() {
        let dir = temp_dir("lock-order");
        let own = REGISTRY_LOCK.replace("1.0.0", "1.0.9");
        let config = packaged_owner(&dir, "[package]\nname = \"owner\"\n", &own, REGISTRY_LOCK);
        let candidates = config.lock_candidates();
        // The workspace's lock first (it lists the package as a dependency), then the
        // package's own, then none.
        assert_eq!(candidates.len(), 3, "{:?}", candidates);
        assert_eq!(candidates[0].as_ref().unwrap().0, dir.join("ws/Cargo.lock"));
        assert_eq!(
            candidates[1].as_ref().unwrap().0,
            dir.join("registry/owner-0.1.0/Cargo.lock")
        );
        assert!(candidates[2].is_none());

        // The same text twice is one attempt.
        let same = packaged_owner(
            &dir,
            "[package]\nname = \"owner\"\n",
            REGISTRY_LOCK,
            REGISTRY_LOCK,
        );
        assert_eq!(same.lock_candidates().len(), 2);

        // A lock file that has the package only as its own member (as `cargo package`'s
        // verification, or any workspace that holds it, has it) is not "the workspace
        // that builds it as a dependency": the package's own lock is the one then.
        let member = REGISTRY_LOCK.replace("source = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"00\"\n", "");
        let config = packaged_owner(&dir, "[package]\nname = \"owner\"\n", &own, &member);
        let candidates = config.lock_candidates();
        assert_eq!(candidates.len(), 2, "{:?}", candidates);
        assert_eq!(
            candidates[0].as_ref().unwrap().0,
            dir.join("registry/owner-0.1.0/Cargo.lock")
        );
        // The versions that the build resolves are what `::name` is matched against.
        let locked = config.lock_for_specs();
        assert!(locked.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_packaged_crate_gets_a_workspace_of_its_own_and_its_lock_file_is_left_alone() {
        let dir = temp_dir("shadow");
        let manifest =
            "[package]\nname = \"owner\"\nedition = \"2018\"\n\n[dependencies]\nleaf = \"1\"\n";
        let config = packaged_owner(&dir, manifest, "# the package's own lock\n", REGISTRY_LOCK);
        let package = config.manifest_dir.clone();
        let own_lock = std::fs::read_to_string(package.join("Cargo.lock")).unwrap();

        let shadow = match config.nested_workspace() {
            NestedWorkspace::Shadow(shadow) => shadow,
            NestedWorkspace::InPlace => panic!("a packaged crate runs in a workspace of its own"),
        };
        // In the build's profile directory, not in the package.
        assert!(shadow
            .manifest
            .starts_with(dir.join("ws/target/debug/subcrate-owner-deps/owner")));
        let root = std::fs::read_to_string(&shadow.manifest).unwrap();
        assert!(root.contains("members = [\"pkg\"]"), "{}", root);
        // The resolver of an edition 2018 package as a root.
        assert!(root.contains("resolver = \"1\""), "{}", root);
        assert!(!root.contains("[patch"), "{}", root);
        let member = shadow.manifest.parent().unwrap().join("pkg");
        assert_eq!(std::fs::read_link(&member).unwrap(), package);
        assert_eq!(shadow.lock.parent(), shadow.manifest.parent());

        // The package's own lock file is not Cargo's to read or write.
        assert_eq!(
            std::fs::read_to_string(package.join("Cargo.lock")).unwrap(),
            own_lock
        );

        // A second look finds the same thing (and does not need to set it up again).
        match config.nested_workspace() {
            NestedWorkspace::Shadow(again) => assert_eq!(again.manifest, shadow.manifest),
            NestedWorkspace::InPlace => panic!("the workspace changed"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_packaged_crates_nested_build_runs_in_its_private_workspace_on_each_lock_file_in_turn() {
        let dir = temp_dir("shadow-run");
        let config = packaged_owner(
            &dir,
            "[package]\nname = \"owner\"\n\n[dependencies]\nleaf = \"1\"\n",
            "# the package's own lock\n",
            REGISTRY_LOCK,
        );
        let package = config.manifest_dir.clone();
        let mut config = config;
        config.cargo = PathBuf::from("/bin/sh");
        // A stand-in for Cargo that fails while it has a lock file to read (logging
        // the first line of it), and builds without one.
        let message = concat!(
            r#"{"reason":"compiler-artifact","package_id":"#,
            r#""registry+https://github.com/rust-lang/crates.io-index#leaf@1.0.0","#,
            r#""manifest_path":"/r/leaf/Cargo.toml","target":{"kind":["lib"],"name":"leaf"},"#,
            r#""filenames":["/t/debug/deps/libleaf-1.rlib","/t/debug/deps/libleaf-1.rmeta"]}"#
        );
        std::fs::write(package.join("messages.txt"), message).unwrap();
        std::fs::write(
            package.join("build"),
            "prev=\nfor a in \"$@\"; do\n  [ \"$prev\" = --manifest-path ] && m=\"$a\"\n  prev=\"$a\"\ndone\n\
             echo \"$m\" >> manifest.txt\n\
             d=$(dirname \"$m\")\n\
             if [ -f \"$d/Cargo.lock\" ]; then\n  head -n 1 \"$d/Cargo.lock\" >> locks.txt\n  echo boom >&2\n  exit 1\nfi\n\
             echo none >> locks.txt\ncat messages.txt\n",
        )
        .unwrap();
        let subs = [top("a", vec![ext("leaf")])];
        let links = resolve_links(&subs, &owner()).unwrap();
        let deps = config.build_owner_deps(&subs, &links, &[true]).unwrap();
        assert!(deps.crates.contains_key("leaf"));

        // Each attempt: the workspace's lock (here "version = 3", a different first
        // line from the package's own), the package's own, and none.
        let locks = std::fs::read_to_string(package.join("locks.txt")).unwrap();
        assert_eq!(
            locks.lines().collect::<Vec<_>>(),
            ["version = 3", "# the package's own lock", "none"]
        );
        let manifests = std::fs::read_to_string(package.join("manifest.txt")).unwrap();
        let manifest = manifests.lines().next().unwrap();
        assert!(manifests.lines().all(|line| line == manifest));
        assert!(
            manifest.ends_with("Cargo.toml") && manifest.contains("subcrate-owner-deps"),
            "{}",
            manifest
        );
        // And the package's own lock file is as it was.
        assert_eq!(
            std::fs::read_to_string(package.join("Cargo.lock")).unwrap(),
            "# the package's own lock\n"
        );

        // When every attempt fails, the first one's error is the one reported.
        std::fs::write(package.join("build"), "echo first >&2\nexit 1\n").unwrap();
        let message = config
            .build_owner_deps(&subs, &links, &[true])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(message.contains("--manifest-path"), "{}", message);
        assert!(message.contains("first"), "{}", message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_package_that_stands_alone_is_given_a_workspace_of_its_own() {
        let dir = temp_dir("shadow-not");
        let in_place = |manifest: &str, marker: bool| {
            let config = packaged_owner(&dir, manifest, "", REGISTRY_LOCK);
            if !marker {
                std::fs::remove_file(config.manifest_dir.join("Cargo.toml.orig")).unwrap();
            }
            matches!(config.nested_workspace(), NestedWorkspace::InPlace)
        };
        let plain = "[package]\nname = \"owner\"\n\n[dependencies]\nleaf = \"1\"\n";
        // Not packaged: a source tree, its workspace's (or its own) lock file applies.
        assert!(in_place(plain, false));
        // Packaged, but a manifest that needs something around it.
        assert!(in_place("[package]\nname = \"owner\"\n[workspace]\n", true));
        assert!(in_place(
            "[package]\nname = \"owner\"\n[dependencies]\nleaf = { workspace = true }\n",
            true
        ));
        assert!(in_place(
            "[package]\nname = \"owner\"\n[dependencies]\nleaf = { path = \"../leaf\" }\n",
            true
        ));
        // An absolute path finds its target from anywhere.
        let absolute = format!(
            "[package]\nname = \"owner\"\n[dependencies]\nleaf = {{ path = {} }}\n",
            toml_quote(&std::env::temp_dir())
        );
        assert!(!cfg!(unix) || !in_place(&absolute, true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_private_workspace_names_the_resolver_the_package_would_have_as_a_root() {
        let manifest = |edition: &str, extra: &str| {
            format!(
                "[package]\nname = \"owner\"\nedition = \"{}\"\n{}",
                edition, extra
            )
        };
        let resolver = |text: String, edition: &str| {
            let root = shadow_manifest(&text, edition, &[]);
            let line = root
                .lines()
                .find(|line| line.starts_with("resolver"))
                .unwrap()
                .to_string();
            line
        };
        assert_eq!(resolver(manifest("2015", ""), "2015"), "resolver = \"1\"");
        assert_eq!(resolver(manifest("2018", ""), "2018"), "resolver = \"1\"");
        assert_eq!(resolver(manifest("2021", ""), "2021"), "resolver = \"2\"");
        assert_eq!(resolver(manifest("2024", ""), "2024"), "resolver = \"3\"");
        // Its own word wins; one in another table is not it.
        assert_eq!(
            resolver(manifest("2018", "resolver = \"2\"\n"), "2018"),
            "resolver = \"2\""
        );
        assert_eq!(
            resolver(manifest("2018", "[lints]\nresolver = \"3\"\n"), "2018"),
            "resolver = \"1\""
        );
    }

    #[test]
    fn a_package_verified_next_to_its_siblings_is_patched_to_their_extracted_sources() {
        let dir = temp_dir("overlay");
        let package_dir = dir.join("target/package");
        let make = |name: &str| {
            let sibling = package_dir.join(name);
            std::fs::create_dir_all(&sibling).unwrap();
            std::fs::write(sibling.join("Cargo.toml"), "").unwrap();
            sibling
        };
        let owner = make("owner-0.1.0");
        let leaf = make("leaf-1.0.0");
        let _unrelated = make("other-3.0.0");
        let mut config = config();
        config.manifest_dir = owner.clone();
        // Without Cargo's overlay beside the packages it is not a verification.
        assert!(config.overlay_patches(REGISTRY_LOCK).is_empty());
        std::fs::create_dir_all(package_dir.join("tmp-registry")).unwrap();
        // `leaf` is a registry dependency whose extracted source is there. The package
        // itself, and a package from another registry, are not patched.
        let other = REGISTRY_LOCK.replace(
            "registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"11",
            "registry+https://example.com/index\"\nchecksum = \"11",
        );
        assert_eq!(
            config.overlay_patches(REGISTRY_LOCK),
            vec![("leaf".to_string(), leaf.clone())]
        );
        assert!(config.overlay_patches(&other).is_empty());
        // A package that was not extracted is left to Cargo's registry.
        let gone = REGISTRY_LOCK.replace("1.0.0", "1.0.1");
        assert!(config.overlay_patches(&gone).is_empty());

        let text = shadow_manifest(
            "[package]\n",
            "2021",
            &config.overlay_patches(REGISTRY_LOCK),
        );
        assert!(
            text.contains("[patch.crates-io]\nleaf = { path = \""),
            "{}",
            text
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_is_quoted_for_toml() {
        assert_eq!(toml_quote(Path::new("/a/b")), "\"/a/b\"");
        assert_eq!(toml_quote(Path::new("C:\\a\\\"b")), "\"C:\\\\a\\\\\\\"b\"");
    }

    #[test]
    fn a_nested_build_variable_is_for_the_one_package_it_names() {
        // A name of its own: the real variables are not touched.
        let name = "SUBCRATE_TEST_SCOPED_VARIABLE";
        let here = scope_key(Path::new("/w/here"));
        std::env::remove_var(name);
        assert!(!env_is_for(name, &here));
        std::env::set_var(name, &here);
        assert!(env_is_for(name, &here));
        // Set for another package, as it is in the build scripts of that package's
        // dependencies: not for this one.
        assert!(!env_is_for(name, &scope_key(Path::new("/w/there"))));
        // And the value `1` the variables used to have (for everyone) names no one.
        std::env::set_var(name, "1");
        assert!(!env_is_for(name, &here));
        std::env::remove_var(name);
    }

    #[test]
    fn a_package_is_named_by_its_canonical_manifest_directory() {
        let dir = std::env::temp_dir().join(format!("subcrate-scope-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        // However Cargo happens to spell the directory, it is one package.
        assert_eq!(
            scope_key(&dir.join("pkg").join("..").join("pkg")),
            scope_key(&dir.join("pkg"))
        );
        assert_ne!(scope_key(&dir.join("pkg")), scope_key(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_nested_build_is_set_up_for_this_package_alone() {
        let leaf = |manifest: &str| {
            let subs = [sub("tests/t.rs", &["user"], vec![ext("leaf")])];
            nested_build("scope", manifest, MULTI_LOCK, "", &subs)
        };
        // Another package's nested build sets the variables for *its* build scripts, and
        // this package's build script runs inside it, as a dependency.
        let other = scope_key(Path::new("/w/other"));
        std::env::set_var(NESTED_ENV, &other);
        std::env::set_var(HARVEST_ONLY_ENV, &other);

        // The owner joins the nested build only to unify a path dependency's features:
        // both variables, for this package.
        let path = "[package]\nname = \"owner\"\n\n[dependencies]\nleaf = { path = \"../leaf\" }\n";
        let (result, args, _) = leaf(path);
        result.unwrap();
        let line = |args: &[String], name: &str| {
            let prefix = format!("{}=", name);
            args.iter()
                .find_map(|arg| arg.strip_prefix(prefix.as_str()))
                .unwrap()
                .to_string()
        };
        // The nested build runs in the package's directory: the one the variables name.
        let this = line(&args, "PWD");
        assert_eq!(line(&args, NESTED_ENV), this);
        assert_eq!(line(&args, HARVEST_ONLY_ENV), this);

        // A registry dependency does not need the owner: nothing to harvest, so the
        // other package's value is not passed on.
        let registry = "[package]\nname = \"owner\"\n\n[dependencies]\nleaf = \"1\"\n";
        let (result, args, _) = leaf(registry);
        result.unwrap();
        let this = line(&args, "PWD");
        assert_eq!(line(&args, NESTED_ENV), this);
        assert_eq!(line(&args, HARVEST_ONLY_ENV), "unset");

        // The build for the host, of the crates a proc-macro subcrate uses, is the same
        // nested build of this package.
        let host = |manifest: &str| {
            let mut proc_macro = top("a", vec![ext("leaf")]);
            proc_macro.proc_macro = true;
            nested_host_build("scope", manifest, MULTI_LOCK, "", &[proc_macro])
        };
        let (result, args, _) = host(path);
        result.unwrap();
        let this = line(&args, "PWD");
        assert_eq!(line(&args, NESTED_ENV), this);
        assert_eq!(line(&args, HARVEST_ONLY_ENV), this);
        let (result, args, _) = host(registry);
        result.unwrap();
        let this = line(&args, "PWD");
        assert_eq!(line(&args, NESTED_ENV), this);
        assert_eq!(line(&args, HARVEST_ONLY_ENV), "unset");

        std::env::remove_var(NESTED_ENV);
        std::env::remove_var(HARVEST_ONLY_ENV);
    }

    #[test]
    fn the_search_paths_of_build_scripts_are_read_from_cargos_messages() {
        // `backed`'s build script has its subcrates in `out/subcrates` (a kindless
        // path), and its own nested build's directories as `dependency` paths.
        let message = concat!(
            r#"{"reason":"build-script-executed","package_id":"path+file:///w/backed#0.1.0","#,
            r#""linked_libs":[],"linked_paths":["/t/debug/build/backed-1/out/subcrates","#,
            r#""dependency=/t/debug/subcrate-owner-deps/backed/1.99/debug","#,
            r#""native=/usr/lib","framework=/Library/Frameworks"],"cfgs":[],"env":[],"#,
            r#""out_dir":"/t/debug/build/backed-1/out"}"#,
        );
        let quiet = r#"{"reason":"build-script-executed","package_id":"x","linked_libs":[],"linked_paths":[],"cfgs":[],"env":[],"out_dir":"/t/out"}"#;
        let deps = parse_artifacts(&format!("{}\n{}\n", quiet, message));
        let paths: Vec<&Path> = deps.script_paths.iter().map(PathBuf::as_path).collect();
        // The kind is dropped, a framework directory is not a place for crates, and
        // the message is not an artifact.
        assert_eq!(
            paths,
            [
                Path::new("/t/debug/build/backed-1/out/subcrates"),
                Path::new("/t/debug/subcrate-owner-deps/backed/1.99/debug"),
                Path::new("/usr/lib"),
            ]
        );
        assert!(deps.crates.is_empty() && deps.lib_dirs.is_empty());
    }

    #[test]
    fn a_path_is_inside_a_directory_by_its_components() {
        assert!(is_inside(Path::new("/t/a/b/c"), Path::new("/t/a")));
        assert!(is_inside(Path::new("/t/a"), Path::new("/t/a")));
        assert!(!is_inside(Path::new("/t/ab"), Path::new("/t/a")));
        assert!(!is_inside(Path::new("/usr/lib"), Path::new("/t/a")));
    }

    #[cfg(unix)]
    #[test]
    fn only_the_search_paths_into_the_nested_build_are_kept() {
        let dir =
            std::env::temp_dir().join(format!("subcrate-nested-scripts-{}", std::process::id()));
        // Where `nested_build` has the nested build go (the compiler is `rustc`'s).
        let target = shared_target_dir(
            &dir.join("target/debug/build/owner-1a/out"),
            "owner",
            rustc_key(Path::new("rustc")).as_deref(),
        );
        let messages = format!(
            concat!(
                r#"{{"reason":"build-script-executed","package_id":"path+file:///w/backed#0.1.0","#,
                r#""linked_libs":[],"linked_paths":["{inside}/x/out/subcrates","native=/usr/lib"],"#,
                r#""cfgs":[],"env":[],"out_dir":"{inside}/x/out"}}"#
            ),
            inside = target.display()
        );
        let registry = "[package]\nname = \"owner\"\n\n[dependencies]\nleaf = \"1\"\n";
        let subs = [sub("tests/t.rs", &["user"], vec![ext("leaf")])];
        let (result, _, _) = nested_build("scripts", registry, MULTI_LOCK, &messages, &subs);
        let paths: Vec<PathBuf> = result.unwrap().script_paths.into_iter().collect();
        // What the build's own scripts put in its target directory, not a system path.
        assert_eq!(paths, [target.join("x/out/subcrates")]);
    }

    #[cfg(unix)]
    #[test]
    fn only_the_search_paths_into_the_host_build_are_kept() {
        let dir =
            std::env::temp_dir().join(format!("subcrate-host-scripts-{}", std::process::id()));
        // Where `nested_host_build` has the nested build go: its own directory, beside the
        // target's (whose paths are another build's, not this one's).
        let out_dir = dir.join("target/aarch64-unknown-linux-gnu/debug/build/owner-1a/out");
        let compiler = rustc_key(Path::new("rustc")).map(|key| format!("{}-host", key));
        let host = shared_target_dir(&out_dir, "owner", compiler.as_deref());
        let target = shared_target_dir(&out_dir, "owner", rustc_key(Path::new("rustc")).as_deref());
        assert_ne!(host, target);
        let message = |inside: &Path| {
            format!(
                concat!(
                    r#"{{"reason":"build-script-executed","package_id":"path+file:///w/backed#0.1.0","#,
                    r#""linked_libs":[],"linked_paths":["{inside}/x/out/subcrates","native=/usr/lib"],"#,
                    r#""cfgs":[],"env":[],"out_dir":"{inside}/x/out"}}"#
                ),
                inside = inside.display()
            )
        };
        let messages = format!("{}\n{}\n", message(&host), message(&target));
        let registry = "[package]\nname = \"owner\"\n\n[dependencies]\nleaf = \"1\"\n";
        let mut proc_macro = top("a", vec![ext("leaf")]);
        proc_macro.proc_macro = true;
        let (result, _, _) =
            nested_host_build("scripts", registry, MULTI_LOCK, &messages, &[proc_macro]);
        let paths: Vec<PathBuf> = result.unwrap().script_paths.into_iter().collect();
        assert_eq!(paths, [host.join("x/out/subcrates")]);
    }

    #[test]
    fn subcrates_search_the_dependencies_subcrate_paths_of_both_builds_when_cross_compiling() {
        let mut config = cross_config();
        config.owner_deps = external_artifacts();
        config
            .owner_deps
            .script_paths
            .insert(PathBuf::from("t/debug/build/backed/h/out/subcrates"));
        let mut host = external_artifacts();
        host.lib_dirs = [PathBuf::from("host/deps")].into_iter().collect();
        host.script_paths = [
            // The host build's own, and a search path of its artifacts already.
            PathBuf::from("host/build/backed/h/out/subcrates"),
            PathBuf::from("host/deps"),
        ]
        .into_iter()
        .collect();
        config.host_deps = host;
        let mut proc_macro = top("a", vec![]);
        proc_macro.proc_macro = true;
        let searched = |sub: &Subcrate| -> Vec<String> {
            let on_owner_dep = ExternDep {
                owner_linked: true,
                ..linked("__subcrate_b_1", "b")
            };
            config
                .rustc_args(sub, &[on_owner_dep], Path::new("out/subcrates"))
                .into_iter()
                .filter(|arg| arg.starts_with("dependency="))
                .collect()
        };
        // The proc-macro subcrate, built on the host's crates, finds what the host's
        // build scripts asked for; and so does a library subcrate, which may use it.
        for sub in [&proc_macro, &top("a", vec![])] {
            let searched = searched(sub);
            assert_eq!(
                searched,
                [
                    "dependency=out/subcrates",
                    "dependency=t/debug",
                    "dependency=t/debug/build/leaf/h/out",
                    "dependency=host/deps",
                    "dependency=host/build/backed/h/out/subcrates",
                    "dependency=t/debug/build/backed/h/out/subcrates",
                ],
                "{:?}",
                searched
            );
        }
    }

    #[test]
    fn subcrates_built_on_owner_dependencies_search_the_dependencies_subcrate_paths() {
        let mut config = config();
        config.owner_deps = external_artifacts();
        config
            .owner_deps
            .script_paths
            .insert(PathBuf::from("t/debug/build/backed/h/out/subcrates"));
        // One a search path of the artifacts already: not given twice.
        config
            .owner_deps
            .script_paths
            .insert(PathBuf::from("t/debug"));
        let on_owner_dep = ExternDep {
            owner_linked: true,
            ..linked("__subcrate_b_1", "b")
        };
        let args = config.rustc_args(
            &top("a", vec![]),
            &[on_owner_dep],
            Path::new("out/subcrates"),
        );
        let searched: Vec<&String> = args
            .iter()
            .filter(|arg| arg.starts_with("dependency="))
            .collect();
        assert_eq!(
            searched,
            [
                "dependency=out/subcrates",
                "dependency=t/debug",
                "dependency=t/debug/build/leaf/h/out",
                "dependency=t/debug/build/backed/h/out/subcrates",
            ]
        );
        // A subcrate that links nothing built by Cargo does not look there.
        let args = config.rustc_args(
            &top("a", vec![]),
            &[linked("__subcrate_b_1", "b")],
            Path::new("out/subcrates"),
        );
        assert!(!args.iter().any(|arg| arg.contains("backed")), "{:?}", args);
    }

    /// A lock file with the owner, its dependencies and an unrelated package.
    const LOCK: &str = "version = 3

[[package]]
name = \"owner\"
version = \"0.1.0\"
dependencies = [
 \"a-b\",
 \"a_b\",
 \"proc-macro2\",
 \"serde 1.0.0\",
 \"syn 2.0.0 (registry+https://github.com/rust-lang/crates.io-index)\",
]

[[package]]
name = \"proc-macro2\"
version = \"1.0.0\"
dependencies = [
 \"unicode-ident\",
]

[[package]]
name = \"unrelated\"
version = \"0.1.0\"
dependencies = [
 \"serde\",
 \"extra\",
]

[[package]]
name = \"extra\"
version = \"0.1.0\"
";

    /// The package names of a locked dependency list.
    fn names(dependencies: &[LockedDep]) -> Vec<&str> {
        dependencies.iter().map(|dep| dep.name.as_str()).collect()
    }

    #[test]
    fn the_dependencies_of_the_package_are_read_from_the_lock_file() {
        let dependencies = locked_dependencies(LOCK, "owner").unwrap();
        assert_eq!(
            names(&dependencies),
            ["a-b", "a_b", "proc-macro2", "serde", "syn"]
        );
        // The version, where the lock file spells one out.
        let versions: Vec<Option<&str>> = dependencies
            .iter()
            .map(|dep| dep.version.as_deref())
            .collect();
        assert_eq!(versions, [None, None, None, Some("1.0.0"), Some("2.0.0")]);
        // Only this package's own list, not that of the packages around it.
        let unrelated = locked_dependencies(LOCK, "unrelated").unwrap();
        assert_eq!(names(&unrelated), ["serde", "extra"]);
        // A package without dependencies has an empty list; one not in the lock, none.
        assert!(locked_dependencies(LOCK, "extra").unwrap().is_empty());
        assert_eq!(locked_dependencies(LOCK, "absent"), None);
    }

    /// A declared dependency on `package`, with nothing to pick its version by.
    fn declare(package: &str) -> Declared {
        Declared {
            package: package.to_string(),
            pin: None,
        }
    }

    /// The spec `::name` resolves to, for a package of `LOCK`/`MULTI_LOCK` with no
    /// help from the manifest.
    fn spec(lock: &str, package: &str, name: &str) -> Result<String, String> {
        let locked = locked_dependencies(lock, package).unwrap();
        owner_package(name, None, Some(&locked)).map(|package| package.spec)
    }

    #[test]
    fn a_crate_name_is_matched_to_a_package_up_to_dashes() {
        assert_eq!(spec(LOCK, "owner", "serde").as_deref(), Ok("serde"));
        assert_eq!(
            spec(LOCK, "owner", "proc_macro2").as_deref(),
            Ok("proc-macro2")
        );
        // An exact match beats one that differs in dashes.
        assert_eq!(spec(LOCK, "owner", "a_b").as_deref(), Ok("a_b"));
        assert_eq!(spec(LOCK, "owner", "a-b").as_deref(), Ok("a-b"));
        // Not a dependency of the owner, even though it is in the lock file.
        assert!(spec(LOCK, "owner", "extra").is_err());
        assert!(spec(LOCK, "owner", "notadep").is_err());
        // Dashes are not interchangeable with anything else.
        assert!(spec(LOCK, "owner", "proc_macro_2").is_err());
        // The crate is named for the package, with underscores.
        let locked = locked_dependencies(LOCK, "owner").unwrap();
        let package = owner_package("proc_macro2", None, Some(&locked)).unwrap();
        assert_eq!(package.crate_name, "proc_macro2");
        assert_eq!(package.version, None);
    }

    /// A lock file with two versions of `dup`: the owner uses the second, and `mid`
    /// (which the owner uses) the first. `leaf` is there once.
    const MULTI_LOCK: &str = "version = 3

[[package]]
name = \"dup\"
version = \"1.0.0\"

[[package]]
name = \"dup\"
version = \"2.0.0\"

[[package]]
name = \"leaf\"
version = \"0.1.0\"

[[package]]
name = \"mid\"
version = \"0.1.0\"
dependencies = [
 \"dup 1.0.0\",
]

[[package]]
name = \"owner\"
version = \"0.1.0\"
dependencies = [
 \"dup 2.0.0\",
 \"leaf\",
 \"mid\",
]

[[package]]
name = \"twice\"
version = \"0.1.0\"
dependencies = [
 \"dup 1.0.0\",
 \"dup 2.0.0\",
]
";

    #[test]
    fn the_locked_version_tells_two_versions_of_a_package_apart() {
        let locked = locked_dependencies(MULTI_LOCK, "owner").unwrap();
        let copies: Vec<(&str, usize)> = locked
            .iter()
            .map(|dep| (dep.name.as_str(), dep.copies))
            .collect();
        assert_eq!(copies, [("dup", 2), ("leaf", 1), ("mid", 1)]);

        // `-p dup` would be ambiguous, so the version the owner resolves to is added
        // (as `name:version`, the spec form that Cargo 1.56 also takes).
        let package = owner_package("dup", None, Some(&locked)).unwrap();
        assert_eq!(package.spec, "dup:2.0.0");
        assert_eq!(package.version.as_deref(), Some("2.0.0"));
        assert_eq!(package.crate_name, "dup");
        // Another package resolves the same name to the other version.
        assert_eq!(spec(MULTI_LOCK, "mid", "dup").as_deref(), Ok("dup:1.0.0"));
        // A package of which there is one copy keeps its plain name.
        let package = owner_package("leaf", None, Some(&locked)).unwrap();
        assert_eq!(package.spec, "leaf");
        assert_eq!(package.version, None);
        assert_eq!(spec(MULTI_LOCK, "owner", "mid").as_deref(), Ok("mid"));
    }

    #[test]
    fn a_renamed_dependency_on_a_package_with_two_versions_gets_its_version() {
        let locked = locked_dependencies(MULTI_LOCK, "owner").unwrap();
        let package = owner_package("dup_two", Some(&declare("dup")), Some(&locked)).unwrap();
        assert_eq!(package.spec, "dup:2.0.0");
        assert_eq!(package.crate_name, "dup");
    }

    #[test]
    fn two_versions_of_a_package_among_the_owners_own_dependencies_need_a_version() {
        // Which one `::dup` means cannot be told from the lock file alone.
        let message = spec(MULTI_LOCK, "twice", "dup").unwrap_err();
        assert!(
            message.contains("several versions of `dup` (1.0.0, 2.0.0)"),
            "{}",
            message
        );
        assert!(message.contains("`::dup`"), "{}", message);

        // The version the manifest declares for the name tells.
        let locked = locked_dependencies(MULTI_LOCK, "twice").unwrap();
        let pinned = |pin: Pin| {
            let declared = Declared {
                package: "dup".to_string(),
                pin: Some(pin),
            };
            owner_package("dup", Some(&declared), Some(&locked))
        };
        let requirement = |text: &str| pinned(Pin::Requirement(text.to_string()));
        assert_eq!(requirement("1").unwrap().spec, "dup:1.0.0");
        assert_eq!(requirement("^2.0").unwrap().spec, "dup:2.0.0");
        assert_eq!(requirement("2").unwrap().version.as_deref(), Some("2.0.0"));
        assert_eq!(
            pinned(Pin::Exact("2.0.0".to_string())).unwrap().spec,
            "dup:2.0.0"
        );
        // A requirement both versions satisfy, or neither, tells nothing.
        assert!(requirement(">=1").is_err());
        assert!(requirement("3").is_err());
        assert!(pinned(Pin::Exact("3.0.0".to_string())).is_err());
    }

    #[test]
    fn cargo_version_requirements_are_matched() {
        let cases = [
            // (requirement, version, satisfied)
            ("1", "1.4.2", true),
            ("1", "2.0.0", false),
            ("1.2", "1.2.9", true),
            ("1.2", "1.3.0", true),
            ("1.2", "1.1.9", false),
            ("^1.2.3", "1.2.3", true),
            ("^1.2.3", "1.2.2", false),
            ("^1.2.3", "2.0.0", false),
            ("0.2", "0.2.7", true),
            ("0.2", "0.3.0", false),
            ("0.2.3", "0.2.9", true),
            ("0.0.3", "0.0.3", true),
            ("0.0.3", "0.0.4", false),
            ("0", "0.9.9", true),
            ("0", "1.0.0", false),
            ("~1.2.3", "1.2.9", true),
            ("~1.2.3", "1.3.0", false),
            ("~1", "1.9.0", true),
            ("~1", "2.0.0", false),
            ("=1.2.3", "1.2.3", true),
            ("=1.2.3", "1.2.4", false),
            ("=1.2", "1.2.8", true),
            (">=1.2.3", "1.2.3", true),
            (">=1.2.3", "1.2.2", false),
            (">1.2.3", "1.2.3", false),
            (">1.2.3", "1.2.4", true),
            (">1.2", "1.2.9", false),
            (">1.2", "1.3.0", true),
            ("<2", "1.9.9", true),
            ("<2", "2.0.0", false),
            ("<=2", "2.9.9", true),
            ("<=2.0.0", "2.0.1", false),
            (">=1.2, <1.5", "1.4.0", true),
            (">=1.2, <1.5", "1.5.0", false),
            ("1.*", "1.7.0", true),
            ("1.*", "2.0.0", false),
            ("1.2.*", "1.2.5", true),
            ("1.2.*", "1.3.0", false),
            ("*", "7.7.7", true),
            // Pre-release and build suffixes are ignored.
            ("1", "1.0.0-beta.1", true),
            ("2", "1.0.0+build", false),
        ];
        for (requirement, version, expected) in cases {
            assert_eq!(
                version_matches(requirement, version),
                expected,
                "{} {}",
                requirement,
                version
            );
        }
        // Nonsense matches nothing.
        assert!(!version_matches("a.b", "1.0.0"));
        assert!(!version_matches("1", "1.0"));
    }

    #[test]
    fn a_path_dependency_is_pinned_to_the_version_on_disk() {
        let dir = std::env::temp_dir().join(format!("subcrate-pin-test-{}", std::process::id()));
        let old = dir.join("old");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(
            old.join("Cargo.toml"),
            "[package]\nname = \"dup\"\nversion = \"1.0.0\" # old\n\n[dependencies]\nversion = \"9\"\n",
        )
        .unwrap();
        let manifest = "[dependencies]\n\
                        dup = { path = \"../dup2\", version = \"2\" }\n\
                        old = { package = \"dup\", path = \"old\" }\n\
                        req = { package = \"dup\", version = \"~1.0\" }\n\
                        plain = \"3\"\n\
                        bare = { package = \"b\" }\n";
        let declared = declared_dependencies(manifest);
        let mut config = config();
        config.manifest_dir = dir.clone();
        let declared_for = |name: &str| config.declared(find_declared(&declared, name).unwrap());
        // On disk (when it can be read): that exact version.
        assert_eq!(
            declared_for("old").pin,
            Some(Pin::Exact("1.0.0".to_string()))
        );
        // Else the requirement, plain or in a table.
        assert_eq!(
            declared_for("dup").pin,
            Some(Pin::Requirement("2".to_string()))
        );
        assert_eq!(
            declared_for("req").pin,
            Some(Pin::Requirement("~1.0".to_string()))
        );
        assert_eq!(
            declared_for("plain").pin,
            Some(Pin::Requirement("3".to_string()))
        );
        assert_eq!(declared_for("bare").pin, None);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            package_version("[package]\nversion.workspace = true\n"),
            None
        );
    }

    #[test]
    fn without_a_lock_file_the_manifest_or_the_name_is_all_there_is() {
        // The package is the one the manifest says, or the name itself; Cargo checks.
        let package = owner_package("myleaf", Some(&declare("leaf-base")), None).unwrap();
        assert_eq!(package.spec, "leaf-base");
        assert_eq!(package.crate_name, "leaf_base");
        let package = owner_package("some_dep", None, None).unwrap();
        assert_eq!(package.spec, "some_dep");
        // A declared dependency that the (stale) lock file lacks is passed on, too.
        let locked = locked_dependencies(MULTI_LOCK, "owner").unwrap();
        let package = owner_package("new", Some(&declare("new-pkg")), Some(&locked)).unwrap();
        assert_eq!(package.spec, "new-pkg");
        // ...but a name the manifest does not declare, and the lock file lacks, is not.
        let message = owner_package("new", None, Some(&locked)).unwrap_err();
        assert_eq!(message, "`new` is not a dependency of this package");
    }

    /// A manifest with dependencies declared in every shape.
    const MANIFEST: &str = r#"[package]
name = "owner"
version = "0.1.0"

[dependencies]
plain = "1"
myleaf = { package = "leaf", path = "../leaf" } # a rename
my-base = { path = "../base#x", package = 'leaf-base', features = ["a", "b"] }
dotted.package = "dotted-real"
dotted.version = "2"
inherited = { workspace = true }
other.workspace = true
tricky = { version = "1", features = ["package = \"x\""] }

[dependencies.long_form]
package = "long-real"
version = "1"
features = [
    "a",
]

[dev-dependencies]
dev_only = { package = "dev-real", version = "1" }
shared = { package = "dev-shared" }

[build-dependencies]
shared = { package = "build-shared" }
build_only = "1"

[target.'cfg(unix)'.dependencies]
unix_only = { package = "unix-real" }

[target.'cfg(windows)'.dependencies.win_only]
package = "win-real"

[features]
package = []

[workspace.dependencies]
inherited = { package = "inherited-real", version = "1" }
other = "1"
"#;

    /// What the manifest declares `name` to be: the real package, as `Config` would
    /// resolve it without a workspace to look into.
    fn declared(name: &str) -> Option<String> {
        let declared = declared_dependencies(MANIFEST);
        find_declared(&declared, name).map(|dep| dep.package.clone().unwrap_or(dep.key.clone()))
    }

    #[test]
    fn a_renamed_dependency_is_found_by_its_key_in_the_manifest() {
        // The key is what the package's code calls the crate; `package` is the crate.
        assert_eq!(declared("myleaf").as_deref(), Some("leaf"));
        // Not renamed: the key is the package.
        assert_eq!(declared("plain").as_deref(), Some("plain"));
        assert_eq!(declared("build_only").as_deref(), Some("build_only"));
        // Single quotes, a `#` in a string, and fields next to `package`.
        assert_eq!(declared("my_base").as_deref(), Some("leaf-base"));
        // The key matches as written, or with `-` for `_`; the real name is as declared.
        assert_eq!(declared("my-base").as_deref(), Some("leaf-base"));
        // The other table forms.
        assert_eq!(declared("dotted").as_deref(), Some("dotted-real"));
        assert_eq!(declared("long_form").as_deref(), Some("long-real"));
        assert_eq!(declared("dev_only").as_deref(), Some("dev-real"));
        assert_eq!(declared("unix_only").as_deref(), Some("unix-real"));
        assert_eq!(declared("win_only").as_deref(), Some("win-real"));
        // `package` inside a string, a feature called `package` and a package that
        // is only declared for the workspace are not renames.
        assert_eq!(declared("tricky").as_deref(), Some("tricky"));
        assert_eq!(declared("package"), None);
        assert_eq!(declared("absent"), None);
    }

    #[test]
    fn the_ordinary_dependency_wins_over_a_dev_or_build_one_of_the_same_name() {
        // `[dev-dependencies]` before `[build-dependencies]`, though the build
        // one comes later in the file and the dev one earlier.
        assert_eq!(declared("shared").as_deref(), Some("dev-shared"));
        let manifest = "[build-dependencies]\nx = { package = \"b\" }\n\
                        [dev-dependencies]\nx = { package = \"d\" }\n\
                        [dependencies]\nx = { package = \"n\" }\n";
        let declared = declared_dependencies(manifest);
        let found = find_declared(&declared, "x").unwrap();
        assert_eq!(found.package.as_deref(), Some("n"));
        assert_eq!(found.table, DependencyTable::Normal);
    }

    #[test]
    fn dependencies_are_inherited_from_the_workspace() {
        let declared = declared_dependencies(MANIFEST);
        let inherited = find_declared(&declared, "inherited").unwrap();
        assert!(inherited.workspace && inherited.package.is_none());
        assert!(find_declared(&declared, "other").unwrap().workspace);
        // The workspace's own table is never a dependency of the package itself ...
        assert!(declared
            .iter()
            .any(|dep| dep.table == DependencyTable::Workspace));
        // ... and holds the real package.
        let dir = std::env::temp_dir().join(format!("subcrate-ws-test-{}", std::process::id()));
        let member = dir.join("member");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(dir.join("Cargo.toml"), MANIFEST).unwrap();
        std::fs::write(member.join("Cargo.toml"), "[package]\nname = \"m\"\n").unwrap();
        let inherited = workspace_dependency(&member, "inherited").unwrap();
        assert_eq!(inherited.package.as_deref(), Some("inherited-real"));
        assert_eq!(inherited.version.as_deref(), Some("1"));
        let other = workspace_dependency(&member, "other").unwrap();
        assert_eq!((other.package, other.version.as_deref()), (None, Some("1")));
        assert_eq!(workspace_dependency(&member, "absent"), None);
        let mut config = config();
        config.manifest_dir = member;
        let package = config.declared(find_declared(&declared, "inherited").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(package.package, "inherited-real");
        // The workspace's version requirement is what picks among versions.
        assert_eq!(package.pin, Some(Pin::Requirement("1".to_string())));
    }

    #[test]
    fn toml_comments_and_keys_are_read_outside_strings_only() {
        assert_eq!(strip_toml_comment("a = \"x#y\" # c"), "a = \"x#y\" ");
        assert_eq!(strip_toml_comment("a = 'x#y'#c"), "a = 'x#y'");
        assert_eq!(strip_toml_comment("a = \"q\\\"#\"#c"), "a = \"q\\\"#\"");
        assert_eq!(strip_toml_comment("a = 1"), "a = 1");
        assert_eq!(split_key_value("a = \"b=c\" "), Some(("a ", " \"b=c\" ")));
        assert_eq!(
            split_toml_key("a.\"b.c\".d-e").unwrap(),
            ["a", "b.c", "d-e"]
        );
        assert_eq!(split_toml_key("\"x"), None);
        assert_eq!(split_toml_key("a b"), None);
        assert_eq!(
            inline_table_field("{ a = [1, 2], b = \"x\" }", "b"),
            Some("\"x\"")
        );
        assert_eq!(inline_table_field("{ a = [1, 2] }", "b"), None);
        assert_eq!(inline_table_field("\"1.0\"", "b"), None);
    }

    #[test]
    fn explicit_target_paths_are_read_from_the_manifest() {
        let manifest = "\
[package]
name = \"x\"
path = \"not-a-target.rs\"

[lib]
path = \"src/the_lib.rs\"

[[bin]]
name = \"tool\"
path = \"bin/tool.rs\"

[[test]]
name = \"it\"
path = \"../shared/it.rs\"

[[example]]
# a comment
path = \"examples/weird.rs\"

[dependencies]
path = \"ignored\"
";
        // `[lib]`/`[[bin]]`/`[[test]]`/`[[example]]` paths, in order; a `path` key
        // in a non-target section (`[package]`, `[dependencies]`) is ignored.
        let paths: Vec<String> = declared_targets(manifest)
            .into_iter()
            .filter_map(|target| target.path)
            .collect();
        assert_eq!(
            paths,
            [
                "src/the_lib.rs",
                "bin/tool.rs",
                "../shared/it.rs",
                "examples/weird.rs",
            ]
        );
        // The library's own path, for the owner crate.
        assert_eq!(lib_source_path(manifest).as_deref(), Some("src/the_lib.rs"));
        assert_eq!(lib_source_path("[package]\nname = \"p\"\n"), None);
    }

    fn target(kind: &str, name: Option<&str>, path: Option<&str>) -> DeclaredTarget {
        DeclaredTarget {
            kind: kind.to_string(),
            name: name.map(str::to_string),
            path: path.map(str::to_string),
        }
    }

    #[test]
    fn declared_targets_carry_their_name_and_path() {
        let manifest = "\
[package]
name = \"x\"

[lib]
name = \"x_lib\"

[[test]]
name = \"by_name\"

[[test]]
name = \"elsewhere\"
path = \"suite/elsewhere.rs\" # a comment
harness = false

[[bin]]
path = \"src/nameless.rs\"

[features]
name = \"not-a-target\"

[[bench]]
name = \"last\"
";
        assert_eq!(
            declared_targets(manifest),
            [
                target("lib", Some("x_lib"), None),
                target("test", Some("by_name"), None),
                target("test", Some("elsewhere"), Some("suite/elsewhere.rs")),
                target("bin", None, Some("src/nameless.rs")),
                target("bench", Some("last"), None),
            ]
        );
        assert_eq!(declared_targets("[package]\nname = \"p\"\n"), []);
        assert_eq!(package_name(manifest).as_deref(), Some("x"));
    }

    #[test]
    fn target_discovery_is_on_unless_the_manifest_turns_it_off() {
        let on = AutoDiscovery::default();
        assert_eq!(auto_discovery(""), on);
        assert_eq!(auto_discovery("[package]\nname = \"p\"\n"), on);
        assert_eq!(
            auto_discovery("[package]\nautotests = true # the default\nautobins = true\n"),
            on
        );
        // Each flag is its own: only the kind it names is off.
        let off = |manifest: &str| auto_discovery(&format!("[package]\n{}\n", manifest));
        let flags = |auto: AutoDiscovery| [auto.bins, auto.tests, auto.benches, auto.examples];
        assert_eq!(flags(off("autobins = false")), [false, true, true, true]);
        assert_eq!(flags(off("autotests = false")), [true, false, true, true]);
        assert_eq!(flags(off("autobenches=false")), [true, true, false, true]);
        assert_eq!(
            flags(off("autoexamples = false # no")),
            [true, true, true, false]
        );
        // Only `[package]` says so.
        assert_eq!(
            auto_discovery("[dependencies]\nautotests = false\n[package]\nname = \"p\"\n"),
            on
        );
        // The directories to watch follow.
        assert_eq!(on.root_dirs(), ["src", "tests", "benches", "examples"]);
        assert_eq!(
            off("autotests = false").root_dirs(),
            ["src", "benches", "examples"]
        );
        assert_eq!(
            auto_discovery(
                "[package]\nautotests = false\nautobenches = false\nautoexamples = false"
            )
            .root_dirs(),
            ["src"]
        );
    }

    /// A package in a temporary directory: its `Cargo.toml` and the given (empty)
    /// files, and the file names `crate_roots` finds, relative to it, in order.
    fn roots_of(test: &str, manifest: &str, files: &[&str]) -> Vec<String> {
        let dir =
            std::env::temp_dir().join(format!("subcrate-roots-{}-{}", test, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), manifest).unwrap();
        for file in files {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "").unwrap();
        }
        let mut roots: Vec<String> = crate_roots(&dir)
            .into_iter()
            .map(|(file, _)| {
                file.strip_prefix(&dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        roots.sort();
        roots
    }

    const LAYOUT: &[&str] = &[
        "src/lib.rs",
        "src/main.rs",
        "src/bin/a.rs",
        "src/bin/b/main.rs",
        "tests/t.rs",
        "tests/u/main.rs",
        "benches/b.rs",
        "examples/e.rs",
        "suite/s.rs",
    ];

    #[test]
    fn target_roots_are_found_by_their_place_by_default() {
        let all = [
            "benches/b.rs",
            "examples/e.rs",
            "src/bin/a.rs",
            "src/bin/b/main.rs",
            "src/lib.rs",
            "src/main.rs",
            "tests/t.rs",
            "tests/u/main.rs",
        ];
        assert_eq!(
            roots_of("default", "[package]\nname = \"p\"\n", LAYOUT),
            all
        );
        // `true` is the default, and the flags are `[package]` keys alone.
        assert_eq!(
            roots_of(
                "true",
                "[package]\nname = \"p\"\nautotests = true\n[dependencies]\nautobins = false\n",
                LAYOUT
            ),
            all
        );
    }

    #[test]
    fn target_discovery_that_is_off_leaves_only_the_declared_targets() {
        // The tests, benchmarks, examples and binaries are the package's own only if
        // it declares them: files in those places are otherwise someone else's (the
        // tests of a workspace's root package, in a package of their own in `tests/`).
        let off = "[package]\nname = \"p\"\nautotests = false\nautobenches = false\n\
                   autoexamples = false\nautobins = false\n";
        assert_eq!(roots_of("off", off, LAYOUT), ["src/lib.rs"]);

        // Each flag leaves the others alone.
        assert_eq!(
            roots_of(
                "tests-off",
                "[package]\nname = \"p\"\nautotests = false\n",
                LAYOUT
            ),
            [
                "benches/b.rs",
                "examples/e.rs",
                "src/bin/a.rs",
                "src/bin/b/main.rs",
                "src/lib.rs",
                "src/main.rs",
            ]
        );

        // The declared ones are roots, with or without a `path`, whatever the
        // directory: `[[test]]`, `[[bench]]` and `[[example]]` by name (as `.rs` or
        // `main.rs` in a directory of that name), a `[[bin]]` named like the package
        // by `src/main.rs`, any other by `src/bin/<name>`.
        let declared = format!(
            "{}{}",
            off,
            "[[test]]\nname = \"t\"\n[[test]]\nname = \"u\"\n\
             [[test]]\nname = \"elsewhere\"\npath = \"suite/s.rs\"\n\
             [[bench]]\nname = \"b\"\n[[example]]\nname = \"e\"\n\
             [[bin]]\nname = \"p\"\n[[bin]]\nname = \"b\"\n\
             [[test]]\nname = \"missing\"\n"
        );
        assert_eq!(
            roots_of("declared", &declared, LAYOUT),
            [
                "benches/b.rs",
                "examples/e.rs",
                "src/bin/b/main.rs",
                "src/lib.rs",
                "src/main.rs",
                "suite/s.rs",
                "tests/t.rs",
                "tests/u/main.rs",
            ]
        );
        // A binary of another name than the package's is not `src/main.rs`.
        let other = format!("{}{}", off, "[[bin]]\nname = \"a\"\n");
        assert_eq!(
            roots_of("bin", &other, LAYOUT),
            ["src/bin/a.rs", "src/lib.rs"]
        );
    }

    #[test]
    fn the_nested_build_shares_one_target_directory_per_package_profile_and_compiler() {
        let dir = |out: &str, package: &str, compiler: Option<&str>| -> PathBuf {
            shared_target_dir(Path::new(out), package, compiler)
        };
        let shared = Path::new("/w/target/debug/subcrate-owner-deps/pkg/1.98.1-797e8a9bc");
        let compiler = Some("1.98.1-797e8a9bc");
        // Whichever configuration of the package it is, and whichever way Cargo lays
        // the `build` directory out (a `<package>-<hash>` directory or `<package>/<hash>`).
        assert_eq!(
            dir("/w/target/debug/build/pkg-1a/out", "pkg", compiler),
            shared
        );
        assert_eq!(
            dir("/w/target/debug/build/pkg-2b/out", "pkg", compiler),
            shared
        );
        assert_eq!(
            dir("/w/target/debug/build/pkg/3c/out", "pkg", compiler),
            shared
        );
        // Another package, profile, target or compiler gets its own.
        assert_eq!(
            dir("/w/target/debug/build/other-1a/out", "other", compiler),
            Path::new("/w/target/debug/subcrate-owner-deps/other/1.98.1-797e8a9bc")
        );
        assert_eq!(
            dir("/w/target/release/build/pkg-1a/out", "pkg", compiler),
            Path::new("/w/target/release/subcrate-owner-deps/pkg/1.98.1-797e8a9bc")
        );
        assert_eq!(
            dir(
                "/w/target/aarch64-unknown-linux-gnu/debug/build/pkg-1a/out",
                "pkg",
                compiler
            ),
            Path::new(
                "/w/target/aarch64-unknown-linux-gnu/debug/subcrate-owner-deps/pkg/1.98.1-797e8a9bc"
            )
        );
        assert_eq!(
            dir(
                "/w/target/debug/build/pkg-1a/out",
                "pkg",
                Some("1.56.0-09c42c458")
            ),
            Path::new("/w/target/debug/subcrate-owner-deps/pkg/1.56.0-09c42c458")
        );
        // A directory called `build` further up does not matter ...
        assert_eq!(
            dir("/build/w/target/debug/build/pkg-1a/out", "pkg", None),
            Path::new("/build/w/target/debug/subcrate-owner-deps/pkg")
        );
        // ... and without one, the nested build stays in `OUT_DIR`.
        assert_eq!(
            dir("/w/elsewhere/out", "pkg", compiler),
            Path::new("/w/elsewhere/out/owner-deps-target")
        );
    }

    #[test]
    fn an_owner_deps_key_overrides_the_package_in_the_shared_dir() {
        // Default: the directory component is the package name.
        let cfg = config();
        assert_eq!(cfg.owner_deps_dir_key(), "owner");
        // Set: the key replaces it.
        assert_eq!(
            config().owner_deps_key("ws-shared").owner_deps_dir_key(),
            "ws-shared"
        );
        // The key is sanitized to one path component, so it cannot escape the
        // directory (no separators, no `..`).
        assert_eq!(
            config()
                .owner_deps_key("../evil/key.x")
                .owner_deps_dir_key(),
            "___evil_key_x"
        );
        // An all-stripped key still yields a valid component.
        assert_eq!(config().owner_deps_key("/").owner_deps_dir_key(), "_");
    }

    #[test]
    fn the_dedup_lock_is_exclusive_and_released_on_drop() {
        let dir = std::env::temp_dir().join(format!("subcrate-deduplock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lockfile = dir.join(".subcrate-dedup.lock");

        let held =
            DedupLock::acquire_with(&dir, Duration::from_millis(60), Duration::from_secs(30));
        assert!(held.is_some());
        assert!(lockfile.exists());
        // A second holder cannot take it while it is held and fresh: it waits out
        // the short deadline and gives up.
        assert!(
            DedupLock::acquire_with(&dir, Duration::from_millis(60), Duration::from_secs(30))
                .is_none()
        );
        drop(held);
        assert!(!lockfile.exists(), "the lock file is removed on drop");

        // A lock left behind by a crashed build (older than the stale timeout) is
        // stolen rather than waited on forever.
        std::fs::write(&lockfile, b"").unwrap();
        let stolen =
            DedupLock::acquire_with(&dir, Duration::from_millis(60), Duration::from_millis(0));
        assert!(stolen.is_some(), "a stale lock is stolen");
        drop(stolen);
        assert!(!lockfile.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_compiler_is_named_by_its_release_and_commit() {
        // The running compiler has one (its name is whatever this toolchain is).
        let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
        let key = rustc_key(Path::new(&rustc)).unwrap();
        assert!(key.starts_with(|c: char| c.is_ascii_digit()), "{}", key);
        assert!(
            key.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'),
            "{}",
            key
        );
        assert_eq!(rustc_key(Path::new("/nonexistent/rustc")), None);
    }

    /// A `compiler-artifact` message in the shape of current Cargo (the uplifted
    /// rlib and the rmeta in a per-package directory).
    const SERDE_MESSAGE: &str = concat!(
        r#"{"reason":"compiler-artifact","package_id":"registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0","#,
        r#""manifest_path":"/home/u/.cargo/registry/src/x/serde-1.0.0/Cargo.toml","#,
        r#""target":{"kind":["lib"],"crate_types":["lib"],"name":"serde","src_path":"/x/lib.rs","edition":"2018"},"#,
        r#""profile":{"opt_level":"0"},"features":["std"],"#,
        r#""filenames":["/t/debug/libserde.rlib","/t/debug/build/serde/ab/out/libserde-ab.rmeta"],"executable":null,"fresh":false}"#
    );

    /// The same for a path dependency, with the shape of older Cargo (everything in
    /// `deps`, and a package id of the form `name version (source)`).
    const LEAF_MESSAGE: &str = concat!(
        r#"{"reason":"compiler-artifact","package_id":"leaf 0.1.0 (path+file:///w/examples/leaf)","#,
        r#""manifest_path":"/w/examples/leaf/Cargo.toml","#,
        r#""target":{"kind":["lib"],"crate_types":["lib"],"name":"leaf","src_path":"/w/examples/leaf/src/lib.rs","edition":"2021"},"#,
        r#""filenames":["/t/debug/deps/libleaf-cd.rlib","/t/debug/deps/libleaf-cd.rmeta"],"executable":null,"fresh":true}"#
    );

    #[test]
    fn artifacts_are_read_from_cargos_json_messages() {
        let stdout = format!(
            "{}\n{}\n{}\n{}\n",
            // Not an artifact: ignored.
            r#"{"reason":"build-script-executed","package_id":"x","out_dir":"/t/out"}"#,
            SERDE_MESSAGE,
            LEAF_MESSAGE,
            r#"{"reason":"build-finished","success":true}"#
        );
        let deps = parse_artifacts(&stdout);

        let serde = &deps.crates["serde"];
        assert_eq!(
            serde.rlib.as_deref(),
            Some(Path::new("/t/debug/libserde.rlib"))
        );
        assert_eq!(
            serde.rmeta.as_deref(),
            Some(Path::new("/t/debug/build/serde/ab/out/libserde-ab.rmeta"))
        );
        let leaf = &deps.crates["leaf"];
        assert_eq!(
            leaf.rlib.as_deref(),
            Some(Path::new("/t/debug/deps/libleaf-cd.rlib"))
        );
        assert_eq!(
            leaf.rmeta.as_deref(),
            Some(Path::new("/t/debug/deps/libleaf-cd.rmeta"))
        );
        assert_eq!(deps.crates.len(), 2);

        // Every directory holding an artifact is a search path.
        let dirs: Vec<&Path> = deps.lib_dirs.iter().map(PathBuf::as_path).collect();
        assert_eq!(
            dirs,
            [
                Path::new("/t/debug"),
                Path::new("/t/debug/build/serde/ab/out"),
                Path::new("/t/debug/deps"),
            ]
        );
    }

    /// One `compiler-artifact` message for a build of `syn 2.0.0` with the given
    /// features (a comma-separated list of quoted names) and an rlib/rmeta named by
    /// `hash`, so different builds are told apart.
    fn syn_build(features: &str, hash: &str) -> String {
        format!(
            concat!(
                r#"{{"reason":"compiler-artifact","#,
                r#""package_id":"registry+https://github.com/rust-lang/crates.io-index#syn@2.0.0","#,
                r#""target":{{"kind":["lib"],"name":"syn"}},"features":[{features}],"#,
                r#""filenames":["/t/debug/deps/libsyn-{hash}.rlib","/t/debug/deps/libsyn-{hash}.rmeta"]}}"#
            ),
            features = features,
            hash = hash,
        )
    }

    #[test]
    fn a_feature_superset_build_is_preferred() {
        let key = ("syn".to_string(), "2.0.0".to_string());
        // `[parsing, full]` is a superset of `[parsing]`, so it is the one kept —
        // whichever order Cargo reported the two builds in.
        for stdout in [
            format!(
                "{}\n{}\n",
                syn_build(r#""parsing""#, "aa"),
                syn_build(r#""parsing","full""#, "bb")
            ),
            format!(
                "{}\n{}\n",
                syn_build(r#""parsing","full""#, "bb"),
                syn_build(r#""parsing""#, "aa")
            ),
        ] {
            let deps = parse_artifacts(&stdout);
            let syn = &deps.versions[&key];
            assert_eq!(
                syn.rlib.as_deref(),
                Some(Path::new("/t/debug/deps/libsyn-bb.rlib"))
            );
            assert!(syn.conflict.is_none());
        }
    }

    #[test]
    fn incompatible_feature_builds_are_flagged() {
        // `[parsing, full]` and `[parsing, visit-mut]`: neither contains the other,
        // so the choice is ambiguous and recorded for `external_dep` to report.
        let stdout = format!(
            "{}\n{}\n",
            syn_build(r#""parsing","full""#, "aa"),
            syn_build(r#""parsing","visit-mut""#, "bb"),
        );
        let deps = parse_artifacts(&stdout);
        let syn = &deps.versions[&("syn".to_string(), "2.0.0".to_string())];
        assert!(
            syn.conflict.is_some(),
            "incomparable feature builds should flag a conflict"
        );
    }

    #[test]
    fn a_dash_in_a_target_name_from_older_cargo_is_an_underscore() {
        let line = LEAF_MESSAGE.replace(r#""name":"leaf""#, r#""name":"leaf-base""#);
        let deps = parse_artifacts(&line);
        assert!(deps.crates.contains_key("leaf_base"));
        assert!(!deps.crates.contains_key("leaf-base"));
    }

    /// The artifact message of a `dup` crate in the given version, in the shape of
    /// current Cargo (`new`) or of older Cargo.
    fn dup_message(version: &str, new: bool) -> String {
        let id = if new {
            format!("path+file:///w/dup{}#dup@{}", &version[..1], version)
        } else {
            format!("dup {} (path+file:///w/dup{})", version, &version[..1])
        };
        format!(
            concat!(
                r#"{{"reason":"compiler-artifact","package_id":"{id}","#,
                r#""manifest_path":"/w/dup{major}/Cargo.toml","#,
                r#""target":{{"kind":["lib"],"name":"dup"}},"#,
                r#""filenames":["/t/debug/deps/libdup-{major}.rlib","/t/debug/deps/libdup-{major}.rmeta"]}}"#
            ),
            id = id,
            major = &version[..1]
        )
    }

    #[test]
    fn two_versions_of_a_crate_are_kept_apart() {
        for new in [true, false] {
            let stdout = format!(
                "{}\n{}\n",
                dup_message("1.0.0", new),
                dup_message("2.0.0", new)
            );
            let deps = parse_artifacts(&stdout);
            let rlib = |version: &str| {
                deps.versions[&("dup".to_string(), version.to_string())]
                    .rlib
                    .clone()
            };
            assert_eq!(
                rlib("1.0.0").as_deref(),
                Some(Path::new("/t/debug/deps/libdup-1.rlib"))
            );
            assert_eq!(
                rlib("2.0.0").as_deref(),
                Some(Path::new("/t/debug/deps/libdup-2.rlib"))
            );
            // By name alone, the first one Cargo reported is all there is.
            assert_eq!(
                deps.crates["dup"].rlib.as_deref(),
                Some(Path::new("/t/debug/deps/libdup-1.rlib"))
            );
        }
    }

    #[test]
    fn a_crates_own_rlib_is_preferred_to_the_one_linked_under_its_bare_name() {
        let dir = std::env::temp_dir().join(format!("subcrate-rlib-test-{}", std::process::id()));
        let deps_dir = dir.join("deps");
        std::fs::create_dir_all(&deps_dir).unwrap();
        for file in ["libdup-aa.rlib", "libdup-aa.rmeta", "libdup-bb.rmeta"] {
            std::fs::write(deps_dir.join(file), "").unwrap();
        }
        // Cargo's JSON escapes backslashes, so a Windows path round-trips; mirror
        // that here (an unescaped `\` would be an invalid JSON escape, and the
        // parser would drop it).
        let dir_json = dir.display().to_string().replace('\\', "\\\\");
        // Two versions of `dup`, both linked to `libdup.rlib`; the second of them
        // has no rlib of its own on disk.
        let message = |version: &str, hash: &str| {
            format!(
                concat!(
                    r#"{{"reason":"compiler-artifact","package_id":"path+file:///w/dup#dup@{version}","#,
                    r#""target":{{"kind":["lib"],"name":"dup"}},"#,
                    r#""filenames":["{dir}/libdup.rlib","{dir}/deps/libdup-{hash}.rmeta"]}}"#
                ),
                version = version,
                hash = hash,
                dir = dir_json
            )
        };
        let deps = parse_artifacts(&format!(
            "{}\n{}\n",
            message("1.0.0", "aa"),
            message("2.0.0", "bb")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let rlib = |version: &str| {
            deps.versions[&("dup".to_string(), version.to_string())]
                .rlib
                .clone()
        };
        assert_eq!(rlib("1.0.0"), Some(deps_dir.join("libdup-aa.rlib")));
        assert_eq!(rlib("2.0.0"), Some(dir.join("libdup.rlib")));
        assert_eq!(
            deps.crates["dup"].rlib,
            Some(deps_dir.join("libdup-aa.rlib"))
        );
    }

    #[test]
    fn a_package_id_names_its_version() {
        let version = |id: &str| package_id_version(id);
        // Newer Cargo: `source#name@version`, or `source#version` when the source's
        // last path segment is the name.
        assert_eq!(
            version("registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0").as_deref(),
            Some("1.0.0")
        );
        assert_eq!(
            version("path+file:///w/dup#2.0.0").as_deref(),
            Some("2.0.0")
        );
        // Older Cargo: `name version (source)`.
        assert_eq!(
            version("dup 1.0.0 (path+file:///w/dup1)").as_deref(),
            Some("1.0.0")
        );
        assert_eq!(version("path+file:///w/dup"), None);
        assert_eq!(version("x#"), None);
    }

    #[test]
    fn a_version_picks_among_the_builds_of_a_crate_and_a_rename_picks_the_crate() {
        let stdout = format!(
            "{}\n{}\n",
            dup_message("1.0.0", true),
            dup_message("2.0.0", true)
        );
        let mut config = config();
        config.owner_deps = parse_artifacts(&stdout);
        let sub = top("a", vec![]);
        let expect = |config: &Config, name: &str, rlib: &str| {
            let dep = config.external_dep(&sub, name, "alias").unwrap();
            // Linked under the name written, whichever crate that is ...
            assert_eq!(dep.internal, name);
            assert_eq!(dep.alias, "alias");
            // ... with the files of the version asked for.
            assert_eq!(dep.paths[0], Path::new(rlib));
        };

        // No version asked for (one version in the graph): whatever was built.
        expect(&config, "dup", "/t/debug/deps/libdup-1.rlib");

        // A version asked for, by the name as it is, and by a rename of the crate.
        for name in ["dup", "dup_two"] {
            config.owner_deps.externals.insert(
                name.to_string(),
                ExternalCrate {
                    crate_name: "dup".to_string(),
                    version: Some("2.0.0".to_string()),
                },
            );
            expect(&config, name, "/t/debug/deps/libdup-2.rlib");
        }
        // A rename without a version.
        config.owner_deps.externals.insert(
            "dup_one".to_string(),
            ExternalCrate {
                crate_name: "dup".to_string(),
                version: None,
            },
        );
        expect(&config, "dup_one", "/t/debug/deps/libdup-1.rlib");
        // A rename of a crate that was not built is an error naming the written name.
        config.owner_deps.externals.insert(
            "gone".to_string(),
            ExternalCrate {
                crate_name: "absent".to_string(),
                version: None,
            },
        );
        let message = config
            .external_dep(&sub, "gone", "alias")
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(message.contains("`::gone`"), "{}", message);
    }

    #[test]
    fn only_path_dependencies_are_watched_for_changes() {
        let deps = parse_artifacts(&format!("{}\n{}\n", SERDE_MESSAGE, LEAF_MESSAGE));
        // A registry crate never changes; a path dependency is edited in place. (Its
        // `src` and `build.rs` are only watched if they exist, which they do not here.)
        let watched: Vec<&Path> = deps.sources.iter().map(PathBuf::as_path).collect();
        assert_eq!(watched, [Path::new("/w/examples/leaf/Cargo.toml")]);
    }

    #[test]
    fn artifact_paths_are_unescaped() {
        let line = r#"{"reason":"compiler-artifact","package_id":"x","target":{"name":"c"},"filenames":["C:\\t\\libc.rlib","/t/with \"quote\"/libc.rmeta","/t/\u00e9/p.so"]}"#;
        let deps = parse_artifacts(line);
        assert_eq!(
            deps.crates["c"].rlib.as_deref(),
            Some(Path::new("C:\\t\\libc.rlib"))
        );
        assert_eq!(
            deps.crates["c"].rmeta.as_deref(),
            Some(Path::new("/t/with \"quote\"/libc.rmeta"))
        );
        // A proc-macro's library has no rlib, but its directory is still searched.
        assert!(deps.lib_dirs.contains(Path::new("/t/\u{e9}")));
    }

    #[test]
    fn json_strings_and_arrays_are_scanned() {
        assert_eq!(
            json_string_field(r#"{"a":"x","name":"n\n"}"#, "name").as_deref(),
            Some("n\n")
        );
        assert_eq!(json_string_field(r#"{"a":"x"}"#, "name"), None);
        assert_eq!(
            json_string_array_field(r#"{"f":["a", "b,c" ,"d"],"g":["z"]}"#, "f"),
            ["a", "b,c", "d"]
        );
        assert!(json_string_array_field(r#"{"f":[]}"#, "f").is_empty());
        assert!(json_string_array_field(r#"{"f":1}"#, "g").is_empty());
        // An unterminated string is not a value.
        assert_eq!(json_string(r#"abc"#), None);
    }

    // --- UI testing (`should_error` / `should_warn` / `should_spanned`) ---

    #[test]
    fn json_parses_nested_objects_and_escapes() {
        let value = parse_json(
            r#"{ "level": "error", "n": 12, "ok": true, "none": null,
               "nested": {"text": "a\"b\n"}, "arr": [1, "x", false] }"#,
        )
        .unwrap();
        assert_eq!(value.get("level").and_then(Json::as_str), Some("error"));
        assert_eq!(value.get("n").and_then(Json::as_num), Some(12.0));
        assert_eq!(
            value
                .get("nested")
                .and_then(|n| n.get("text"))
                .and_then(Json::as_str),
            Some("a\"b\n")
        );
        assert!(matches!(value.get("arr"), Some(Json::Arr(items)) if items.len() == 3));
        // Malformed input yields None rather than a panic.
        assert!(parse_json(r#"{"a":}"#).is_none());
        assert!(parse_json(r#"{"a":1"#).is_none());
        assert!(parse_json("not json").is_none());
    }

    #[test]
    fn highlights_are_the_underlined_columns_joined() {
        // One span, one line: columns [16, 19) of the line are `"x"`.
        let span = parse_json(
            r#"{"text":[{"text":"fn f() -> u8 { \"x\" }","highlight_start":16,"highlight_end":19}]}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        collect_highlights(&span, &mut out);
        assert_eq!(out, ["\"x\""]);

        // A multi-line span joins each line's underlined slice.
        let span = parse_json(concat!(
            r#"{"text":[{"text":"foo(","highlight_start":1,"highlight_end":5},"#,
            r#"{"text":"  bar)","highlight_start":1,"highlight_end":7}]}"#,
        ))
        .unwrap();
        let mut out = Vec::new();
        collect_highlights(&span, &mut out);
        assert_eq!(out, ["foo(  bar)"]);

        // A zero-width span (highlight_end == highlight_start) underlines nothing.
        let span = parse_json(r#"{"text":[{"text":"abc","highlight_start":2,"highlight_end":2}]}"#)
            .unwrap();
        let mut out = Vec::new();
        collect_highlights(&span, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn spanned_normalization_ignores_whitespace() {
        assert_eq!(normalize_spanned("x + 1"), "x+1");
        assert_eq!(normalize_spanned("  a\n\tb "), "ab");
        assert_eq!(normalize_spanned("x + 1"), normalize_spanned("x+1"));
    }

    #[test]
    fn whitespace_collapse_folds_runs_but_keeps_word_boundaries() {
        assert_eq!(collapse_whitespace("a  b\n c\t\td"), "a b c d");
        assert_eq!(collapse_whitespace("  trim  me  "), "trim me");
        // A line break inside a sentence becomes a single space.
        assert_eq!(
            collapse_whitespace("versions\nof crate"),
            "versions of crate"
        );
    }

    /// A realistic one-line rustc error diagnostic (`--error-format=json`).
    const ERROR_JSON: &str = concat!(
        r#"{"message":"mismatched types","code":{"code":"E0308"},"level":"error","#,
        r#""spans":[{"file_name":"<anon>","line_start":1,"column_start":16,"column_end":19,"#,
        r#""is_primary":true,"text":[{"text":"fn f() -> u8 { \"x\" }","highlight_start":16,"#,
        r#""highlight_end":19}],"label":"expected `u8`, found `&str`"}],"children":[],"#,
        r#""rendered":"error[E0308]: mismatched types\n --> <anon>:1:16\n"}"#,
    );

    /// A realistic one-line rustc warning diagnostic.
    const WARN_JSON: &str = concat!(
        r#"{"message":"unused variable: `x`","code":{"code":"unused_variables"},"#,
        r#""level":"warning","spans":[{"file_name":"<anon>","line_start":1,"column_start":14,"#,
        r#""column_end":15,"is_primary":true,"text":[{"text":"fn f() { let x = 5; }","#,
        r#""highlight_start":14,"highlight_end":15}],"label":null}],"children":[],"#,
        r#""rendered":"warning: unused variable: `x`\n --> <anon>:1:14\n"}"#,
    );

    #[test]
    fn rustc_json_is_split_into_errors_warnings_and_highlights() {
        let diags = parse_rustc_json(&format!("{}\n{}\n", ERROR_JSON, WARN_JSON));
        assert_eq!(diags.errors.len(), 1);
        assert!(diags.errors[0].contains("mismatched types"));
        assert_eq!(diags.warnings.len(), 1);
        assert!(diags.warnings[0].contains("unused variable"));
        assert!(diags.highlights.contains(&"\"x\"".to_string()));
        assert!(diags.highlights.contains(&"x".to_string()));
        assert!(diags.rendered.contains("error[E0308]"));
        assert!(diags.rendered.contains("warning: unused variable"));
    }

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn opt_strs(items: &[&str]) -> Vec<Option<String>> {
        items.iter().map(|s| Some(s.to_string())).collect()
    }

    #[test]
    fn error_test_passes_when_the_expected_error_and_span_appear() {
        let diags = parse_rustc_json(ERROR_JSON);
        // Compilation failed (success = false) with the expected error and span.
        let problems = check_ui(
            false,
            &diags,
            &opt_strs(&["mismatched types"]),
            &[],
            &strs(&["\"x\""]),
        );
        assert!(problems.is_empty(), "{:?}", problems);
    }

    #[test]
    fn error_test_fails_when_compilation_unexpectedly_succeeds() {
        let diags = parse_rustc_json("");
        let problems = check_ui(true, &diags, &opt_strs(&["mismatched types"]), &[], &[]);
        assert!(problems
            .iter()
            .any(|p| p.contains("expected compilation to fail")));
    }

    #[test]
    fn error_test_fails_when_the_message_or_span_is_absent() {
        let diags = parse_rustc_json(ERROR_JSON);
        let problems = check_ui(
            false,
            &diags,
            &opt_strs(&["some other error"]),
            &[],
            &strs(&["not underlined"]),
        );
        assert!(problems.iter().any(|p| p.contains("some other error")));
        assert!(problems.iter().any(|p| p.contains("not underlined")));
    }

    #[test]
    fn message_match_ignores_rustc_line_wrapping() {
        // `rustc` hard-wraps long messages, so `rendered` can break a sentence across
        // lines (here `versions\nof`). A one-line expectation that spans the break
        // still matches, on both errors and warnings.
        let diags = parse_rustc_json(concat!(
            r#"{"level":"error","spans":[],"children":[],"#,
            r#""rendered":"error: there are multiple different versions\nof crate `foo` in the dependency graph\n"}"#,
            "\n",
            r#"{"level":"warning","spans":[],"children":[],"#,
            r#""rendered":"warning: this is a rather long\nwarning message about something\n"}"#,
        ));
        let problems = check_ui(
            false,
            &diags,
            &opt_strs(&["multiple different versions of crate `foo`"]),
            &opt_strs(&["long warning message about"]),
            &[],
        );
        assert!(problems.is_empty(), "{:?}", problems);

        // A genuinely absent substring still fails (collapsing does not invent text).
        let problems = check_ui(
            false,
            &diags,
            &opt_strs(&["no such text anywhere"]),
            &[],
            &[],
        );
        assert!(problems.iter().any(|p| p.contains("no such text anywhere")));
    }

    #[test]
    fn multiple_non_bare_expectations_are_anded() {
        let diags = parse_rustc_json(ERROR_JSON);
        // Both substrings are present (in the one error) → all hold → pass.
        assert!(check_ui(
            false,
            &diags,
            &opt_strs(&["mismatched types", "E0308"]),
            &[],
            &[]
        )
        .is_empty());
        // One present, one absent → AND fails (it is not OR, which would pass here).
        let problems = check_ui(
            false,
            &diags,
            &opt_strs(&["mismatched types", "nowhere in the output"]),
            &[],
            &[],
        );
        assert!(problems.iter().any(|p| p.contains("nowhere in the output")));
        assert!(!problems.iter().any(|p| p.contains("mismatched types")));
    }

    #[test]
    fn warn_test_passes_on_a_clean_compile_with_the_expected_warning() {
        let diags = parse_rustc_json(WARN_JSON);
        // No `should_error`, compilation succeeded (success = true).
        let problems = check_ui(
            true,
            &diags,
            &[],
            &opt_strs(&["unused variable"]),
            // The span is written with different spacing; normalization matches it.
            &strs(&[" x "]),
        );
        assert!(problems.is_empty(), "{:?}", problems);
    }

    #[test]
    fn bare_should_error_asserts_only_that_compilation_fails() {
        // Bare `should_error` (no substring): a failed compile passes...
        let diags = parse_rustc_json(ERROR_JSON);
        assert!(check_ui(false, &diags, &[None], &[], &[]).is_empty());
        // ...and a successful compile fails the direction check.
        let empty = parse_rustc_json("");
        let problems = check_ui(true, &empty, &[None], &[], &[]);
        assert!(problems
            .iter()
            .any(|p| p.contains("expected compilation to fail")));
    }

    #[test]
    fn bare_should_warn_requires_any_warning() {
        // Bare `should_warn`: a clean compile with a warning passes...
        let warn = parse_rustc_json(WARN_JSON);
        assert!(check_ui(true, &warn, &[], &[None], &[]).is_empty());
        // ...but a clean compile with no warnings does not.
        let clean = parse_rustc_json("");
        let problems = check_ui(true, &clean, &[], &[None], &[]);
        assert!(problems.iter().any(|p| p.contains("at least one warning")));
    }

    #[test]
    fn warn_test_fails_when_compilation_unexpectedly_fails() {
        let diags = parse_rustc_json(ERROR_JSON);
        // A warn-test (no `should_error`) whose subcrate failed to compile.
        let problems = check_ui(false, &diags, &[], &opt_strs(&["unused variable"]), &[]);
        assert!(problems
            .iter()
            .any(|p| p.contains("expected the subcrate to compile")));
    }

    #[test]
    fn a_files_stamp_changes_with_its_content_and_a_missing_file_has_one() {
        let dir = std::env::temp_dir().join(format!("subcrate-stamp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("lib.rlib");
        std::fs::write(&file, "one").unwrap();
        let before = file_stamp(&file);
        std::fs::write(&file, "three").unwrap();
        let after = file_stamp(&file);
        let missing = file_stamp(&dir.join("absent"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_ne!(before, after);
        assert!(missing.ends_with(":missing"), "{}", missing);
    }

    #[test]
    fn two_dependencies_with_one_name_are_an_error() {
        let subs = [
            top("x", vec![]),
            top("y", vec![]),
            top("a", vec![on("x"), dep("x", &["y"])]),
        ];
        let message = order(&subs).unwrap_err();
        assert!(message.contains("more than one dependency named `x`"));
    }

    #[test]
    fn is_artifact_matches_compiled_outputs_only() {
        for name in ["deps/libfoo-1.rlib", "x.rmeta", "x.so", "x.dylib", "x.dll"] {
            assert!(
                is_artifact(Path::new(name)),
                "{} should be an artifact",
                name
            );
        }
        for name in [
            "x.rs",
            "x.d",
            "libfoo",
            // The hard-link temp file (`<name>.subcrate-link-tmp-<pid>`) must not
            // be picked up as an artifact to relink.
            "libfoo-1.rlib.subcrate-link-tmp-42",
        ] {
            assert!(!is_artifact(Path::new(name)), "{} is not an artifact", name);
        }
    }

    #[test]
    fn dedup_hard_links_byte_identical_artifacts() {
        let base = std::env::temp_dir().join(format!("subcrate-dedup-test-{}", std::process::id()));
        let main = base.join("main");
        let side = base.join("side");
        std::fs::create_dir_all(main.join("deps")).unwrap();
        std::fs::create_dir_all(side.join("deps")).unwrap();

        let bytes = b"rlib-bytes-0123456789".repeat(16);
        // A dependency present in both, byte-identical: the reclaimable case.
        std::fs::write(main.join("deps/libdup-abc.rlib"), &bytes).unwrap();
        std::fs::write(side.join("deps/libdup-abc.rlib"), &bytes).unwrap();
        // A side artifact with no twin in main: left alone.
        std::fs::write(side.join("deps/libsolo-xyz.rlib"), b"solo").unwrap();
        // A same-named pair whose contents differ: must not be linked.
        std::fs::write(main.join("deps/libdiff-def.rlib"), b"one").unwrap();
        std::fs::write(side.join("deps/libdiff-def.rlib"), b"two").unwrap();

        dedup_against_main(&side, &main);

        let side_dup = side.join("deps/libdup-abc.rlib");
        let main_dup = main.join("deps/libdup-abc.rlib");
        // The identical twin is now one file with the main copy, and still reads
        // back the same bytes.
        assert!(
            same_file(&side_dup, &main_dup),
            "a byte-identical artifact should be hard-linked to the main copy"
        );
        assert_eq!(std::fs::read(&side_dup).unwrap(), bytes);
        // The crate with no main twin, and the same-named-but-different one, are
        // untouched separate files.
        assert!(!same_file(
            &side.join("deps/libsolo-xyz.rlib"),
            &main.join("deps/libsolo-xyz.rlib")
        ));
        assert_eq!(
            std::fs::read(side.join("deps/libdiff-def.rlib")).unwrap(),
            b"two"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remap_rewrites_anon_to_the_real_file_and_line() {
        // Generated line 3 came from source line 20; line 4 has no mapping.
        let map = vec![None, None, None, Some(20)];
        let stderr = " --> <anon>:3:5\n3 | code\n --> <anon>:4:1\n";
        let out = remap_diagnostics(stderr, &map, "src/lib.rs");
        assert!(out.contains("--> src/lib.rs:20:5"), "{}", out);
        // An unmapped line keeps `<anon>` rather than claim a wrong line.
        assert!(out.contains("<anon>:4:1"), "{}", out);
        // An empty map (an unmappable subcrate) is a no-op.
        assert_eq!(remap_diagnostics(stderr, &[], "src/lib.rs"), stderr);
    }

    #[test]
    fn generated_lines_map_back_to_source_lines() {
        // `BADTOKEN` is on source line 3.
        let src = "fn f() -> u8 {\n    let x = 1;\n    BADTOKEN\n}\n";
        let items = syn::parse_file(src).unwrap().items;
        let (source, map) = render_source(&[], &[], &items, true, false);
        assert!(!map.is_empty());
        let gline = source[..source.find("BADTOKEN").unwrap()]
            .matches('\n')
            .count()
            + 1;
        assert_eq!(
            map.get(gline).copied().flatten(),
            Some(3),
            "source:\n{}\nmap: {:?}",
            source,
            map
        );
    }

    fn is_gate(module_src: &str) -> bool {
        let m: syn::ItemMod = syn::parse_str(module_src).unwrap();
        m.attrs.iter().any(is_rustversion_gate)
    }

    #[test]
    fn rustversion_selectors_are_recognized_as_gates() {
        for src in [
            "#[rustversion::nightly] mod x {}",
            "#[rustversion::stable] mod x {}",
            "#[rustversion::stable(1.34)] mod x {}",
            "#[rustversion::beta] mod x {}",
            "#[rustversion::since(1.70)] mod x {}",
            "#[rustversion::before(1.70)] mod x {}",
            "#[rustversion::not(nightly)] mod x {}",
            "#[rustversion::any(stable, beta)] mod x {}",
            "#[rustversion::all(since(1.31), before(1.34))] mod x {}",
        ] {
            assert!(is_gate(src), "should be a gate: {}", src);
        }
    }

    #[test]
    fn non_gate_attributes_are_not_rustversion_gates() {
        // `attr(...)` adds an attribute rather than removing the item, so it is not
        // a gate; neither is `#[cfg]`, nor a bare `#[nightly]` without the path.
        for src in [
            "#[rustversion::attr(nightly, repr(packed))] mod x {}",
            "#[cfg(test)] mod x {}",
            "#[nightly] mod x {}",
            "#[subcrate] mod x {}",
        ] {
            assert!(!is_gate(src), "should not be a gate: {}", src);
        }
    }
}
