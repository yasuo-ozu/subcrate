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
//! 4. Compile each module, dependencies first, with `rustc` (from `$RUSTC`),
//!    reading the generated source from stdin — no temporary `.rs` files, no
//!    `Cargo.toml`s, no nested Cargo. A dependency is handed to its dependent as
//!    `--extern <internal name>=<artifact>`, plus a `use ::<internal name> as
//!    <alias>;` at the dependent's crate root.
//! 5. Put the artifacts on the library search path and record metadata the
//!    procedural macro reads to confirm it ran.
//!
//! The search path is emitted with `cargo:rustc-link-search` (the legacy
//! single-colon directive form, understood by every Cargo back to the 1.56
//! MSRV; the `cargo::` form is 1.77+). Cargo applies it to every target in this
//! package and, because it is a build-script link path, forwards it to dependent
//! packages' compilations too — so publicly exported subcrates resolve
//! downstream without any extra wiring (requirement 6).

use std::collections::{BTreeSet, HashSet};
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
}

impl Config {
    /// Builds a [`Config`] from the environment variables Cargo sets for build
    /// scripts, so subcrates compile with the same configuration as the package
    /// that owns them: `CARGO_MANIFEST_DIR`, `OUT_DIR`, `RUSTC`, `TARGET`,
    /// `HOST`, `OPT_LEVEL`, `CARGO_CFG_PANIC`, `CARGO_CFG_DEBUG_ASSERTIONS` and
    /// `CARGO_ENCODED_RUSTFLAGS`, plus the package's edition from `Cargo.toml`.
    pub fn from_env() -> Result<Self, Error> {
        let manifest_dir = PathBuf::from(require_env("CARGO_MANIFEST_DIR")?);
        let out_dir = PathBuf::from(require_env("OUT_DIR")?);
        let rustc = PathBuf::from(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()));

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
    pub fn run(self) -> Result<(), Error> {
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
        let links = resolve_links(subcrates)?;
        let order = compile_order(subcrates, &links)?;
        for ix in order {
            let deps = self.extern_deps(subcrates, ix, &links[ix], &subcrates_dir)?;
            self.compile_subcrate(&subcrates[ix], &deps, &subcrates_dir)?;
        }
        let internal_names: Vec<String> = subcrates.iter().map(|s| s.internal.clone()).collect();

        prune_stale(&subcrates_dir, &internal_names);

        // Make the subcrate artifacts resolvable while compiling this package.
        // A kindless search path becomes `-L PATH` (`all` kind), which rustc
        // searches for directly-named `extern crate`s.
        println!("cargo:rustc-link-search={}", subcrates_dir.display());

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

    /// The linked dependencies of `subcrates[ix]` (from its resolved `links`),
    /// with the artifact paths found in `dir`, in a stable order.
    fn extern_deps(
        &self,
        subcrates: &[Subcrate],
        ix: usize,
        links: &[Link],
        dir: &Path,
    ) -> Result<Vec<ExternDep>, Error> {
        let sub = &subcrates[ix];
        let mut deps = Vec::new();
        for link in links {
            let dep = &subcrates[link.target];
            // A proc-macro crate runs on the host, but a library subcrate is built
            // for the target; when those differ the host cannot load it.
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
                path: dir.join(dep.artifact_file()),
            });
        }
        Ok(deps)
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
        let fingerprint = format!("{}\u{1f}{}", args.join("\u{1f}"), source);
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
        let mut linked: HashSet<&str> = HashSet::new();
        for dep in deps {
            // One crate under two aliases is still linked once.
            if linked.insert(&dep.internal) {
                args.push("--extern".into());
                args.push(format!("{}={}", dep.internal, dep.path.display()));
            }
        }

        args.extend(self.extra_rustc_args.iter().cloned());
        args
    }
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
    /// The dependency's internal crate name (what `--extern` names it).
    internal: String,
    /// The name the dependent's source sees it under.
    alias: String,
    /// The dependency's compiled artifact.
    path: PathBuf,
}

/// A resolved dependency: `target` (an index into the discovered subcrates) is
/// linked under the name `alias`.
struct Link {
    alias: String,
    target: usize,
}

/// Resolves every subcrate's declared dependencies to discovered subcrates,
/// returning, for each subcrate (by index), its links sorted by alias.
fn resolve_links(subcrates: &[Subcrate]) -> Result<Vec<Vec<Link>>, Error> {
    let mut all = Vec::with_capacity(subcrates.len());
    for (ix, sub) in subcrates.iter().enumerate() {
        let mut links: Vec<Link> = Vec::new();
        for dep in &sub.deps {
            if dep.external {
                return Err(Error::new(format!(
                    "subcrate `{}`: the dependency `{}` names an external (build-dependency) \
                     crate, which is not implemented yet; the `::name` syntax is reserved for \
                     it. A subcrate can currently only depend on sibling `#[subcrate]` modules",
                    sub.display_name(),
                    dep.path_display()
                )));
            }
            if links.iter().any(|link| link.alias == dep.alias) {
                return Err(Error::new(format!(
                    "subcrate `{}` has more than one dependency named `{}`; \
                     rename one with `alias = path::to::subcrate`",
                    sub.display_name(),
                    dep.alias
                )));
            }
            links.push(Link {
                alias: dep.alias.clone(),
                target: resolve_dependency(subcrates, ix, dep)?,
            });
        }
        links.sort_by(|a, b| a.alias.cmp(&b.alias));
        all.push(links);
    }
    Ok(all)
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
             in this package. (Depending on an external, build-dependency crate this way is \
             reserved syntax and not implemented yet.)",
            subcrates[from].display_name(),
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

/// The dependencies of `subcrates[ix]` the macro can name in the `cfg(doc)`
/// stand-in: those under the same crate root, where `crate::<module path>` is
/// valid. (A dependency from another crate root has no such path.)
fn doc_deps(subcrates: &[Subcrate], ix: usize, links: &[Link]) -> Vec<ResolvedDep> {
    links
        .iter()
        .filter(|link| subcrates[link.target].root == subcrates[ix].root)
        .map(|link| {
            (
                link.alias.clone(),
                subcrates[link.target].module_path.clone(),
            )
        })
        .collect()
}

/// Orders the subcrates (by index) so that each one comes after everything it
/// depends on, using [`safegraph`]'s topological sort. A dependency cycle is an
/// error naming the cycle.
fn compile_order(subcrates: &[Subcrate], links: &[Vec<Link>]) -> Result<Vec<usize>, Error> {
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
    let edges: BTreeSet<(usize, usize)> = links
        .iter()
        .enumerate()
        .flat_map(|(ix, links)| links.iter().map(move |link| (link.target, ix)))
        .collect();
    for (edge, (dependency, dependent)) in edges.into_iter().enumerate() {
        graph
            .insert_edge(edge, [dependency, dependent])
            .map_err(|e| graph_error("edge", &e))?;
    }

    toposort(&graph).map_err(|cycle| {
        let path: Vec<String> = find_cycle(links, cycle.node)
            .into_iter()
            .map(|ix| subcrates[ix].display_name())
            .collect();
        Error::new(format!("subcrate dependency cycle: {}", path.join(" -> ")))
    })
}

/// Finds a path `start -> … -> start` along "depends on" links, for the error
/// message. `start` is known to lie on a cycle, so one exists.
fn find_cycle(links: &[Vec<Link>], start: usize) -> Vec<usize> {
    fn visit(
        links: &[Vec<Link>],
        node: usize,
        start: usize,
        seen: &mut Vec<bool>,
        path: &mut Vec<usize>,
    ) -> bool {
        path.push(node);
        for link in &links[node] {
            if link.target == start {
                path.push(start);
                return true;
            }
            if !seen[link.target] {
                seen[link.target] = true;
                if visit(links, link.target, start, seen, path) {
                    return true;
                }
            }
        }
        path.pop();
        false
    }

    let mut path = Vec::new();
    let mut seen = vec![false; links.len()];
    seen[start] = true;
    visit(links, start, start, &mut seen, &mut path);
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

/// Removes a trailing `# comment` (good enough for the simple lines we read).
fn strip_toml_comment(line: &str) -> &str {
    match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    }
}

/// Extracts the contents of a leading `"…"` string literal, if present.
fn toml_string_value(value: &str) -> Option<String> {
    let value = value.trim();
    let rest = value.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
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

    /// The compile order as module names, or the error message.
    fn order(subs: &[Subcrate]) -> Result<Vec<String>, String> {
        let links = resolve_links(subs).map_err(|e| e.to_string())?;
        let order = compile_order(subs, &links).map_err(|e| e.to_string())?;
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
        let links = resolve_links(&subs).unwrap();
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
        assert!(message.contains("not implemented yet"), "{}", message);
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
        let links = resolve_links(&subs).unwrap();
        assert_eq!(links[2][0].target, 1);
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
        let links = resolve_links(&subs).unwrap();
        assert_eq!(links[2][0].target, 1);
    }

    #[test]
    fn a_dependency_prefers_the_subcrate_under_its_own_crate_root() {
        // Two test files each define their own `helper`; each `user` gets its own.
        let subs = [
            sub("tests/x.rs", &["helper"], vec![]),
            sub("tests/y.rs", &["helper"], vec![]),
            sub("tests/y.rs", &["user"], vec![on("helper")]),
        ];
        let links = resolve_links(&subs).unwrap();
        assert_eq!(links[2][0].target, 1);
    }

    #[test]
    fn a_dependency_may_come_from_another_crate_root() {
        let subs = [
            sub("src/lib.rs", &["shared"], vec![]),
            sub("tests/t.rs", &["user"], vec![on("shared")]),
        ];
        assert_eq!(order(&subs).unwrap(), ["shared", "user"]);
        // ...but `crate::<path>` only names it for the doc stand-in when the roots match.
        let links = resolve_links(&subs).unwrap();
        assert!(doc_deps(&subs, 1, &links[1]).is_empty());
    }

    #[test]
    fn doc_deps_lists_same_root_dependencies_by_alias_and_full_path() {
        let subs = [
            sub("src/lib.rs", &["nested", "leaf"], vec![]),
            top("b", vec![dep("l", &["nested", "leaf"])]),
        ];
        let links = resolve_links(&subs).unwrap();
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
        }
    }

    fn linked(internal: &str, alias: &str) -> ExternDep {
        ExternDep {
            internal: internal.to_string(),
            alias: alias.to_string(),
            path: PathBuf::from(format!("out/subcrates/lib{}.rlib", internal)),
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

    #[test]
    fn external_crates_are_reserved_but_not_supported() {
        let mut external = on("serde");
        external.external = true;
        let message = order(&[top("a", vec![external])]).unwrap_err();
        assert!(message.contains("`::serde`"), "{}", message);
        assert!(message.contains("not implemented yet"), "{}", message);
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
