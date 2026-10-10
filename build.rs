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
//! For finer control, build a [`Config`] with [`Config::from_env`] and tweak it
//! before calling [`Config::run`].
//!
//! # How it works
//!
//! 1. Discover the package's source files starting from the crate roots
//!    (`src/lib.rs`, `src/main.rs`, `src/bin/*.rs`, `tests/*.rs`, `benches/*.rs`,
//!    `examples/*.rs`), following `mod foo;` and `#[path = "…"]` declarations.
//! 2. Parse them and collect every `#[subcrate]` inline module.
//! 3. Resolve the dependencies between subcrates (`#[subcrate(other)]`,
//!    `#[subcrate(alias = path::to::other)]`) and sort them topologically with
//!    [`safegraph`], rejecting dependency cycles.
//! 4. If any subcrate uses an *owner* dependency — a crate the owning package
//!    depends on (`#[subcrate(::serde)]`) or the package's own library
//!    (`#[subcrate(::my_package)]`) — build those with one nested Cargo build
//!    (see [below](#owner-dependencies)).
//! 5. Compile each module, dependencies first, with `rustc` (from `$RUSTC`),
//!    reading the generated source from stdin — no temporary `.rs` files, no
//!    `Cargo.toml`s. A dependency is handed to its dependent as
//!    `--extern <internal name>=<artifact>`, plus a `use ::<internal name> as
//!    <alias>;` at the dependent's crate root.
//! 6. Put the artifacts on the library search path and record metadata the
//!    procedural macro reads to confirm it ran.
//!
//! The search path is emitted with `cargo:rustc-link-search` (the legacy
//! single-colon directive form, understood by every Cargo back to the 1.56
//! MSRV; the `cargo::` form is 1.77+). Cargo applies it to every target in this
//! package and, because it is a build-script link path, forwards it to dependent
//! packages' compilations too — so publicly exported subcrates resolve
//! downstream without any extra wiring (requirement 6).
//!
//! # Owner dependencies
//!
//! A subcrate is compiled by this build script, *before* Cargo builds the
//! package's own dependencies, so none of them exist yet. To let a subcrate use
//! one anyway, the build script asks Cargo to build the ones that are needed
//! (`cargo build -p <dep> … [-p <this package>] --lib`) from the package's
//! manifest directory, into a **separate target directory**. (The build script's
//! own Cargo holds the lock on the main target directory, so sharing it would
//! deadlock.) The directory is `subcrate-owner-deps/<package>/<compiler>` in the
//! profile directory (`target/debug`) that `OUT_DIR` is in, so every configuration
//! of the package — each has an `OUT_DIR` of its own — shares one, and the crates
//! are compiled once. Only the crates a subcrate names are built, and the owner crate
//! only if one of them does: its own build dependencies are compiled then, which is
//! most of the work. Running from the manifest directory makes Cargo
//! resolve versions and features exactly as the outer build does, so the
//! artifacts have the same crate identity as the copies the package itself is
//! compiled against: a type from `serde` means the same thing in the subcrate and
//! in the owner.
//!
//! The separate target directory would otherwise store the owner's dependency
//! chain twice — once there, once in the main target. But Cargo names an artifact
//! by a hash of its content and configuration, so the two copies are byte for byte
//! the same file under the same name. After the nested build the build script
//! replaces each duplicate in the separate directory with a hard link to the main
//! copy (both are in one `target/` tree, hence on one filesystem; a shared hash
//! name can never mean two contents, so the shared inode is safe), reclaiming that
//! disk. It is best effort — a failure just keeps the extra copy.
//!
//! `::name` is the name the package's own code uses for the crate, which
//! `Cargo.toml` says is the dependency's key. For a renamed dependency
//! (`myleaf = { package = "leaf", … }`) the package to build is the `package`, and
//! the subcrate sees its crate under `myleaf`, as the package does. Where the
//! dependency graph holds several versions of a package (so `-p leaf` is
//! ambiguous), the package is selected by the version `Cargo.lock` has this package
//! resolve it to, as `-p leaf:1.2.3`. (That is the spec form every Cargo accepts:
//! `leaf@1.2.3` is newer, and Cargo 1.56 rejects it.) If the owner itself depends on
//! two versions of one package under two names, the `version` (or the `path`) it
//! declares for each name picks the locked version. The artifacts of each version
//! are kept apart, and the crate's own rlib (`deps/lib<name>-<hash>.rlib`) is
//! linked rather than the bare `lib<name>.rlib` Cargo also links it to, which two
//! versions would share.
//!
//! The artifacts are read from Cargo's JSON output and passed to the subcrates
//! that use them as `--extern <name>=<rlib>` *and* `--extern <name>=<rmeta>`
//! (Cargo pipelines compilation, so an rlib alone carries only a metadata stub),
//! with every directory holding an artifact added as a `-L` search path for the
//! dependencies' own dependencies. The same directories become search paths for
//! compiling the package itself, since a subcrate records the exact copy of each
//! crate it was built against and whatever loads the subcrate has to find that
//! copy. (They are the very crates the package is built with under `cargo build`
//! and `cargo test`. Under `cargo check` and clippy Cargo builds other copies, so a
//! *type* from such a crate is a different type there from the subcrate's.)
//!
//! Using the owner crate itself builds the package's library a second time
//! (running this very build script again inside it, with the environment variable
//! `SUBCRATE_NESTED` set). In such a nested build only the library is being built,
//! so the build script compiles just the subcrates the library uses and never
//! starts another owner build. A subcrate in the library cannot use the owner
//! crate — the library would depend on itself — but subcrates in `tests/`,
//! `src/main.rs`, `src/bin/`, `examples/` and `benches/` can.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use safegraph::algo::toposort::toposort;
use safegraph::graph::Graph;
use safegraph::BTreeGraph;
use subcrate_core::{
    internal_crate_name, is_subcrate_attr, manifest_line, parse_attr_args, Dependency, ResolvedDep,
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
    /// The build profile (`PROFILE`): `release` builds the owner's dependencies
    /// with `--release`, anything else without.
    profile: String,
    /// Whether this build script runs inside the nested Cargo build of another
    /// `subcrate` build script (`SUBCRATE_NESTED` is set).
    nested: bool,
    /// The owner's dependencies (and the owner crate), built for the subcrates
    /// that use them. Filled in by [`Config::run`].
    owner_deps: OwnerDeps,
}

/// Environment variable set on the nested Cargo build of the owner's
/// dependencies, so a `subcrate` build script running inside it can tell.
const NESTED_ENV: &str = "SUBCRATE_NESTED";

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
        let rustc = PathBuf::from(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()));
        let cargo = PathBuf::from(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
        let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
        let nested = std::env::var_os(NESTED_ENV).is_some();

        let target = std::env::var("TARGET").ok();
        let host = std::env::var("HOST").ok();
        // Only pass `--target` when genuinely cross-compiling; otherwise the
        // host default already matches and matches how Cargo invokes rustc.
        let cross_target = match (&target, &host) {
            (Some(t), Some(h)) if t != h => Some(t.clone()),
            _ => None,
        };

        let edition = detect_edition(&manifest_dir);
        let panic = std::env::var("CARGO_CFG_PANIC").ok();
        let opt_level = std::env::var("OPT_LEVEL").unwrap_or_else(|_| "0".into());
        // `CARGO_CFG_DEBUG_ASSERTIONS` is present (with an empty value) exactly
        // when the profile enables debug assertions.
        let debug_assertions = std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_some();
        let rustflags = decode_rustflags();
        let feature_cfgs = enabled_feature_cfgs(&manifest_dir);

        Ok(Config {
            manifest_dir,
            out_dir,
            rustc,
            edition,
            target: cross_target,
            panic,
            opt_level,
            debug_assertions,
            rustflags,
            feature_cfgs,
            extra_rustc_args: Vec::new(),
            cargo,
            package,
            profile,
            nested,
            owner_deps: OwnerDeps::default(),
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

    /// Runs discovery and compilation, emitting the necessary Cargo directives.
    pub fn run(mut self) -> Result<(), Error> {
        let subcrates_dir = self.out_dir.join("subcrates");
        std::fs::create_dir_all(&subcrates_dir)?;

        let discovered = discover(&self.manifest_dir);

        // Rebuild whenever the manifest or any discovered source file changes,
        // or when the flags we forward to subcrates change.
        rerun_if_changed(&self.manifest_dir.join("Cargo.toml"));
        // Watch the root source directories too: a Cargo `rerun-if-changed` on a
        // directory fires when files are *added or removed*, so a brand-new file
        // containing a `#[subcrate]` re-runs discovery. (Per-file tracking alone
        // would miss files that did not exist on the previous run.)
        for dir in ["src", "tests", "benches", "examples"] {
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

        let owner_linked = owner_linked(&links, &order);
        for ix in order {
            if !wanted[ix] {
                continue;
            }
            let deps =
                self.extern_deps(subcrates, ix, &links[ix], &subcrates_dir, &owner_linked)?;
            self.compile_subcrate(&subcrates[ix], &deps, &subcrates_dir)?;
        }
        let internal_names: Vec<String> = subcrates.iter().map(|s| s.internal.clone()).collect();

        prune_stale(&subcrates_dir, &internal_names);

        // Make the subcrate artifacts resolvable while compiling this package.
        // A kindless search path becomes `-L PATH` (`all` kind), which rustc
        // searches for directly-named `extern crate`s.
        println!("cargo:rustc-link-search={}", subcrates_dir.display());
        // A subcrate built on an owner dependency records the exact copy of it it
        // was built against, and a crate loading the subcrate (a test, a binary)
        // looks that copy up by name and hash on the search path. Cargo's own search
        // path may not hold it: the package's own library is only handed over with
        // `--extern`, and `cargo check` builds different copies than the
        // `cargo build` ones used here. So point to the copies built here (which are
        // the same crates as the ones the package is built with, otherwise).
        for dir in &self.owner_deps.lib_dirs {
            println!("cargo:rustc-link-search=dependency={}", dir.display());
        }

        // Metadata the procedural macro reads to confirm the helper ran.
        let manifest = self.out_dir.join(MANIFEST_FILE);
        let mut body = String::new();
        for (ix, sub) in subcrates.iter().enumerate() {
            body.push_str(&manifest_line(
                &sub.internal,
                &doc_deps(subcrates, ix, &links[ix]),
            ));
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
        let lib = self.manifest_dir.join("src").join("lib.rs");
        Owner {
            name: self.owner_crate_name(),
            lib: if lib.is_file() { Some(lib) } else { None },
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
                    deps.push(self.external_dep(sub, &self.owner_crate_name(), &link.alias)?);
                }
            }
        }
        Ok(deps)
    }

    /// The link to the crate `name` built by [`Config::build_owner_deps`], seen as
    /// `alias` by `sub`.
    fn external_dep(&self, sub: &Subcrate, name: &str, alias: &str) -> Result<ExternDep, Error> {
        // The crates were built for the target, which a proc-macro (running on the
        // host) cannot load when the two differ.
        if sub.proc_macro && self.target.is_some() {
            return Err(Error::new(format!(
                "proc-macro subcrate `{}` cannot depend on `::{}` when cross-compiling: \
                 it is built for the target, but a proc-macro runs on the host",
                sub.display_name(),
                name
            )));
        }
        // `name` is what the subcrate's author wrote, which is not necessarily the
        // library crate it stands for (a dependency may be renamed), nor the only
        // version of that crate Cargo built.
        let external = self.owner_deps.externals.get(name);
        let crate_name = external.map_or(name, |external| external.crate_name.as_str());
        let krate = external
            .and_then(|external| external.version.as_ref())
            .and_then(|version| {
                let key = (crate_name.to_string(), version.clone());
                self.owner_deps.versions.get(&key)
            })
            .or_else(|| self.owner_deps.crates.get(crate_name))
            .filter(|krate| krate.rlib.is_some())
            .ok_or_else(|| {
                Error::new(format!(
                    "subcrate `{}` depends on `::{}`, but building it produced no library \
                     artifact (only ordinary library crates can be used this way)",
                    sub.display_name(),
                    name
                ))
            })?;
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
    /// directory (see the [module documentation](self)).
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
            if !wanted[ix] {
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

        // Cargo wants package specs (`-p some-crate`), `::name` is a name the package's
        // own code uses: `Cargo.toml` says which package that is (a dependency may be
        // renamed with `package = "…"`), and the lock file which version of it, when
        // there are several (see [`owner_package`]).
        let lock = find_lock(&self.manifest_dir);
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
        for (name, user) in &externals {
            let package = find_declared(&declared, name).map(|dep| self.declared(dep));
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
        if owner_user.is_some() {
            packages.push(self.package.clone());
        }

        let target_dir = self.owner_deps_target_dir();
        let args = self.cargo_args(&packages, &target_dir);
        let mut cmd = Command::new(&self.cargo);
        cmd.args(&args)
            .current_dir(&self.manifest_dir)
            .env(NESTED_ENV, "1")
            .stdin(Stdio::null());
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
        let output = cmd.output().map_err(|e| {
            Error::new(format!(
                "failed to launch `{}`: {}",
                self.cargo.display(),
                e
            ))
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let args: Vec<String> = args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            return Err(Error::new(format!(
                "failed to build the crates the subcrates depend on \
                 (`{} {}` in `{}`):\n{}\n\
                 Every `::name` a `#[subcrate]` uses must be a dependency of this package",
                self.cargo.display(),
                args.join(" "),
                self.manifest_dir.display(),
                indent(stderr.trim_end())
            )));
        }

        // Reclaim the disk the nested build just spent. It re-stored the owner's
        // dependency chain in its own target directory, byte for byte the same as
        // the copies already in the main target: Cargo names an artifact by a hash
        // of its content and configuration, so one name is one content. Replace
        // each duplicate with a hard link to the main copy — both live in the same
        // `target/` tree, hence on the same filesystem, and a shared hash name can
        // never come to mean two different contents, so the shared inode is safe.
        // Best effort: any I/O failure just leaves the extra copy in place.
        if let Some(main) = self.main_profile_dir() {
            dedup_against_main(&target_dir, &main);
        }

        let mut deps = parse_artifacts(&String::from_utf8_lossy(&output.stdout));
        deps.externals = crates;
        // Rebuild the subcrates when the crates they link against change. Cargo
        // does not rerun this build script for that on its own: it only watches
        // what it is told to. A dependency from a registry is immutable, but a
        // path dependency is edited in place, and the resolved versions come from
        // the lock file.
        for source in &deps.sources {
            rerun_if_changed(source);
        }
        if let Some((path, _)) = &lock {
            rerun_if_changed(path);
        }
        Ok(deps)
    }

    /// What the manifest says a dependency `dep` it declares is: the real package —
    /// its `package = "…"` if it is renamed, whatever the workspace declares if it is
    /// inherited (`workspace = true`), else its key — and what picks the version,
    /// should there be several: the version of the package at its `path`, else its
    /// `version` requirement.
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
    /// It has to be one of its own (the build in progress holds the lock on the main
    /// one), but not one per `OUT_DIR`: Cargo gives each configuration of the package
    /// (`cargo build` and `cargo test`, `cargo check`, …) an `OUT_DIR` of its own, and
    /// a nested build into each would compile the same crates over again — the
    /// owner's build-dependencies among them, for the owner crate. The nested build
    /// does not depend on the configuration (it does not see the features or cfgs
    /// of this one, and resolves from the package's `Cargo.toml`), so all of them
    /// share `<profile directory>/subcrate-owner-deps/<package>/<compiler>`: per
    /// package, per profile and target (the profile directory — `target/debug`,
    /// `target/<triple>/release` — is the one `OUT_DIR` sits in) and per compiler.
    /// The last is because Cargo links the crates it builds for the packages asked
    /// for to one name (`libleaf.rlib`, which is what the subcrates are compiled
    /// against), whichever compiler built them, so two compilers sharing a
    /// directory would swap them under each other. Two builds that meet there wait
    /// for each other on that directory's lock, which is not the main target
    /// directory's, so they cannot deadlock.
    ///
    /// Without a `build` directory above `OUT_DIR` to find it by, the nested build
    /// gets a directory in `OUT_DIR` itself.
    fn owner_deps_target_dir(&self) -> PathBuf {
        shared_target_dir(
            &self.out_dir,
            &self.package,
            rustc_key(&self.rustc).as_deref(),
        )
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
    ///   be for an explicit target, which Cargo gives a different identity.
    ///
    /// Cargo's own diagnostics are rendered to stderr (shown if the build fails),
    /// leaving only the artifact messages on stdout as JSON.
    fn cargo_args(&self, packages: &[String], target_dir: &Path) -> Vec<OsString> {
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
        if let Some(target) = &self.target {
            args.push("--target".into());
            args.push(target.into());
        }
        args
    }

    /// The edition `sub` is compiled with: its own override, else the package's.
    fn edition_of<'a>(&'a self, sub: &'a Subcrate) -> &'a str {
        sub.edition.as_deref().unwrap_or(&self.edition)
    }

    fn compile_subcrate(
        &self,
        sub: &Subcrate,
        deps: &[ExternDep],
        out_dir: &Path,
    ) -> Result<(), Error> {
        let args = self.rustc_args(sub, deps, out_dir);
        let source = render_source(&sub.inner_attrs, deps, self.edition_of(sub), &sub.items);

        // Reuse decision: the crate *name* already captures the body as written
        // (a content hash), and the stamp additionally records the exact `rustc`
        // configuration *and the fully rendered source* — the latter so that
        // editing a file inlined via `mod foo;` (whose content is not in the
        // name) still forces a rebuild. We rebuild unless all of it matches. The
        // name itself stays config-independent so the proc macro can recompute it.
        //
        // The configuration includes each `--extern <internal name>=<artifact>`,
        // and a dependency's name embeds that dependency's own content hash, so a
        // changed dependency changes this subcrate's fingerprint and forces it to
        // be recompiled too, even though its own name is unchanged. (The injected
        // `use` of each dependency is part of the rendered source, so it is in the
        // fingerprint but, being a build-time detail, not in the name.)
        //
        // An owner dependency's file name is *not* content-addressed — Cargo's
        // hash covers its package and features, not its source — so the files'
        // sizes and modification times are folded in instead; a rebuilt dependency
        // then recompiles the subcrates linked against it. (The same goes for a
        // sibling subcrate built on one: its name does not cover that either.)
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
        if let Ok(previous) = std::fs::read_to_string(&stamp) {
            if previous == fingerprint {
                return Ok(());
            }
        }

        let mut cmd = Command::new(&self.rustc);
        cmd.arg("-"); // read source from stdin
        for arg in &args {
            cmd.arg(arg);
        }
        cmd.arg("--out-dir").arg(out_dir);

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            Error::new(format!(
                "failed to launch `{}`: {}",
                self.rustc.display(),
                e
            ))
        })?;
        {
            let mut stdin = child.stdin.take().expect("stdin was requested");
            stdin.write_all(source.as_bytes())?;
            // Dropping `stdin` closes the pipe so rustc sees EOF.
        }
        let output = child.wait_with_output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::new(format!(
                "failed to compile subcrate `{}`{}:\n{}",
                sub.display_name(),
                if sub.proc_macro { " (proc-macro)" } else { "" },
                indent(stderr.trim_end())
            )));
        }

        std::fs::write(&stamp, fingerprint)?;
        Ok(())
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
            for dir in &self.owner_deps.lib_dirs {
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
        args
    }
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
        walker.root = file.clone();
        walker.walk_file(&file, &mod_dir);
    }
    walker.out
}

/// Enumerates the crate-root files and the directory their top-level modules
/// resolve against.
fn crate_roots(manifest_dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut roots = Vec::new();
    let src = manifest_dir.join("src");

    for name in ["lib.rs", "main.rs"] {
        let file = src.join(name);
        if file.is_file() {
            roots.push((file, src.clone()));
        }
    }

    // Binaries: `src/bin/*.rs` and `src/bin/<name>/main.rs`.
    collect_root_dir(&src.join("bin"), &mut roots);
    // Integration tests: `tests/*.rs` and `tests/<name>/main.rs`.
    collect_root_dir(&manifest_dir.join("tests"), &mut roots);
    // Benchmarks: `benches/*.rs` and `benches/<name>/main.rs`.
    collect_root_dir(&manifest_dir.join("benches"), &mut roots);
    // Examples: `examples/*.rs` and `examples/<name>/main.rs`.
    collect_root_dir(&manifest_dir.join("examples"), &mut roots);

    roots
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
    /// Module path (from `root`) of the items currently being walked.
    mod_path: Vec<String>,
}

impl Walker {
    fn walk_file(&mut self, path: &Path, mod_dir: &Path) {
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
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
        self.walk_items(&file.items, mod_dir);
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
        self.out.files.extend(extra_files);

        if self.seen_internal.insert(internal.clone()) {
            self.out.subcrates.push(Subcrate {
                module_path,
                root: self.root.clone(),
                deps: args.deps,
                internal,
                proc_macro: args.proc_macro,
                edition: args.edition,
                inner_attrs: inner,
                items: compiled_items,
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
    /// `src/lib.rs`, if the package has a library.
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

/// Resolves a `::name` dependency of `sub`: a crate the package depends on, or —
/// if `name` is the package's own library — the owner crate, which the library's
/// own subcrates cannot use (the library would depend on itself).
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
             (`src/lib.rs`) for it to depend on",
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
/// The import is `use ::<internal name> as <alias>;`, or — in edition 2015, where
/// `::name` means something else — the equivalent `extern crate <internal name>
/// as <alias>;`. It is an injected render-time detail: it is not part of the
/// subcrate's content-addressed name (the dependency *list* in the arguments is).
/// It sits at the crate root, so nested modules reach the alias as
/// `crate::<alias>` or `super::<alias>`.
fn render_source(
    inner_attrs: &[Attribute],
    deps: &[ExternDep],
    edition: &str,
    items: &[Item],
) -> String {
    let attrs = quote! { #(#inner_attrs)* }.to_string();
    let mut imports = String::new();
    for dep in deps {
        // `unused_imports` is allowed: a dependency need not be used.
        if edition == "2015" {
            imports.push_str(&format!(
                "#[allow(unused_extern_crates)] extern crate {} as {};\n",
                dep.internal, dep.alias
            ));
        } else {
            imports.push_str(&format!(
                "#[allow(unused_imports)] use ::{} as {};\n",
                dep.internal, dep.alias
            ));
        }
    }
    let items = quote! { #(#items)* }.to_string();
    format!("{}\n{}{}", attrs, imports, items)
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

/// The dependencies a `Cargo.toml` declares (in `[dependencies]`,
/// `[dev-dependencies]`, `[build-dependencies]` — also under `[target.…]` — and
/// `[workspace.dependencies]`), with whether each is renamed or inherited.
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
    /// The manifests and sources of the path dependencies built, which change
    /// when someone edits them.
    sources: BTreeSet<PathBuf>,
}

/// The artifacts of one crate built by Cargo.
#[derive(Debug, Clone, Default)]
struct OwnerCrate {
    rlib: Option<PathBuf>,
    rmeta: Option<PathBuf>,
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
        for file in json_string_array_field(line, "filenames") {
            let file = PathBuf::from(file);
            let is_rlib = match file.extension().and_then(|ext| ext.to_str()) {
                Some("rlib") => Some(true),
                Some("rmeta") => Some(false),
                // A proc-macro crate: not linkable here, but a library may depend
                // on it, so its directory has to be searched too.
                Some("so") | Some("dylib") | Some("dll") => None,
                _ => continue,
            };
            if let Some(is_rlib) = is_rlib {
                record_artifact(deps.crates.entry(name.clone()).or_default(), is_rlib, &file);
                if let Some(version) = &version {
                    let key = (name.clone(), version.clone());
                    record_artifact(deps.versions.entry(key).or_default(), is_rlib, &file);
                }
            }
            if let Some(dir) = file.parent() {
                deps.lib_dirs.insert(dir.to_path_buf());
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

/// Notes `file`, an rlib or an rmeta, as an artifact of `krate`; the first of each
/// kind wins.
fn record_artifact(krate: &mut OwnerCrate, is_rlib: bool, file: &Path) {
    let slot = if is_rlib {
        &mut krate.rlib
    } else {
        &mut krate.rmeta
    };
    slot.get_or_insert_with(|| file.to_path_buf());
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
            deps,
            internal: format!("__subcrate_{}", path.join("_")),
            proc_macro: false,
            edition: None,
            inner_attrs: Vec::new(),
            items: Vec::new(),
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
            panic: None,
            opt_level: "0".to_string(),
            debug_assertions: true,
            rustflags: Vec::new(),
            feature_cfgs: Vec::new(),
            extra_rustc_args: Vec::new(),
            cargo: PathBuf::from("cargo"),
            package: "owner".to_string(),
            profile: "debug".to_string(),
            nested: false,
            owner_deps: OwnerDeps::default(),
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

        let source = render_source(&[], &deps, "2021", &[]);
        assert!(source.contains("use ::__subcrate_a_1 as a;"), "{}", source);
        assert!(source.contains("use ::__subcrate_c_2 as z;"), "{}", source);
    }

    #[test]
    fn a_crate_under_two_aliases_is_linked_once() {
        let deps = [linked("__subcrate_a_1", "x"), linked("__subcrate_a_1", "y")];
        let args = config().rustc_args(&top("b", vec![]), &deps, Path::new("out/subcrates"));
        assert_eq!(args.iter().filter(|a| *a == "--extern").count(), 1);
        let source = render_source(&[], &deps, "2021", &[]);
        assert!(source.contains("use ::__subcrate_a_1 as x;"));
        assert!(source.contains("use ::__subcrate_a_1 as y;"));
    }

    #[test]
    fn a_2015_edition_subcrate_imports_with_extern_crate() {
        let deps = [linked("__subcrate_a_1", "a")];
        let source = render_source(&[], &deps, "2015", &[]);
        assert!(
            source.contains("extern crate __subcrate_a_1 as a;"),
            "{}",
            source
        );
        assert!(!source.contains("use ::"), "{}", source);
    }

    #[test]
    fn the_injected_import_follows_crate_level_attributes() {
        let file = syn::parse_file("#![allow(dead_code)] fn f() {}").unwrap();
        let source = render_source(
            &file.attrs,
            &[linked("__subcrate_a_1", "a")],
            "2021",
            &file.items,
        );
        let attr = source.find("allow").unwrap();
        let import = source.find("use ::__subcrate_a_1").unwrap();
        let item = source.find("fn f").unwrap();
        assert!(attr < import && import < item, "{}", source);
    }

    #[test]
    fn a_subcrate_without_dependencies_renders_no_import() {
        let args = config().rustc_args(&top("b", vec![]), &[], Path::new("out/subcrates"));
        assert!(!args.contains(&"--extern".to_string()));
        assert!(!args.contains(&"-L".to_string()));
        assert!(!render_source(&[], &[], "2021", &[]).contains("use ::"));
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
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(without.name, "my_package");
        assert_eq!(without.lib, None);
        assert_eq!(with.name, "my_package");
        assert_eq!(with.lib, Some(dir.join("src").join("lib.rs")));
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

        let source = render_source(&[], &deps, "2021", &[]);
        assert!(source.contains("use ::leaf as l;"), "{}", source);
        let source = render_source(&[], &deps, "2015", &[]);
        assert!(source.contains("extern crate leaf as l;"), "{}", source);
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

    #[test]
    fn a_proc_macro_subcrate_cannot_use_an_owner_dependency_when_cross_compiling() {
        let mut config = config();
        config.target = Some("aarch64-unknown-linux-gnu".to_string());
        config.owner_deps = external_artifacts();
        let mut sub = top("a", vec![ext("leaf")]);
        sub.proc_macro = true;
        let links = [Link {
            alias: "leaf".to_string(),
            target: LinkTarget::External("leaf".to_string()),
        }];
        let result = config.extern_deps(&[sub], 0, &links, Path::new("out"), &[true]);
        let message = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(message.contains("cross-compiling"), "{}", message);
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
    /// what it built and the arguments it was given. The package is `owner`, with the
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
        std::fs::write(
            dir.join("build"),
            "printf '%s\\n' \"$@\" > args.txt\ncat messages.txt\n",
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
        assert_eq!(packages_asked_for(&args), ["dup:2.0.0", "leaf"]);
        // Nothing else: not the owner itself.
        assert_eq!(args.iter().filter(|arg| *arg == "-p").count(), 2);

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
    fn the_nested_build_includes_the_owner_only_when_a_subcrate_uses_it() {
        let manifest = "[package]\nname = \"owner\"\n\n[dependencies]\nleaf = \"1\"\n";
        let leaf = |is_owner: bool| {
            if is_owner {
                sub("tests/t.rs", &["user"], vec![ext("leaf"), ext("owner")])
            } else {
                sub("tests/t.rs", &["user"], vec![ext("leaf")])
            }
        };
        let (result, args, _) = nested_build("no-owner", manifest, MULTI_LOCK, "", &[leaf(false)]);
        result.unwrap();
        assert_eq!(packages_asked_for(&args), ["leaf"]);

        let (result, args, _) = nested_build("owner", manifest, MULTI_LOCK, "", &[leaf(true)]);
        result.unwrap();
        assert_eq!(packages_asked_for(&args), ["leaf", "owner"]);
    }

    #[test]
    fn the_nested_cargo_build_is_into_a_separate_target_directory() {
        let args = |config: &Config| -> Vec<String> {
            config
                .cargo_args(
                    &["serde".to_string(), "owner".to_string()],
                    Path::new("out/owner-deps-target"),
                )
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };

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
        let cross = args(&cross);
        let at = cross.iter().position(|a| a == "--target").unwrap();
        assert_eq!(cross[at + 1], "aarch64-unknown-linux-gnu");
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
                dir = dir.display()
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
}
