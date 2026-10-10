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
//! 3. Compile each module with `rustc` (from `$RUSTC`), reading the generated
//!    source from stdin — no temporary `.rs` files, no `Cargo.toml`s, no nested
//!    Cargo.
//! 4. Put the artifacts on the library search path and record metadata the
//!    procedural macro reads to confirm it ran.
//!
//! The search path is emitted with `cargo:rustc-link-search` (the legacy
//! single-colon directive form, understood by every Cargo back to the 1.56
//! MSRV; the `cargo::` form is 1.77+). Cargo applies it to every target in this
//! package and, because it is a build-script link path, forwards it to dependent
//! packages' compilations too — so publicly exported subcrates resolve
//! downstream without any extra wiring (requirement 6).

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use subcrate_core::{internal_crate_name, is_subcrate_attr, parse_attr_args};
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

        let mut internal_names = Vec::new();
        for sub in &discovered.subcrates {
            self.compile_subcrate(sub, &subcrates_dir)?;
            internal_names.push(sub.internal.clone());
        }

        prune_stale(&subcrates_dir, &internal_names);

        // Make the subcrate artifacts resolvable while compiling this package.
        // A kindless search path becomes `-L PATH` (`all` kind), which rustc
        // searches for directly-named `extern crate`s.
        println!("cargo:rustc-link-search={}", subcrates_dir.display());

        // Metadata the procedural macro reads to confirm the helper ran.
        let manifest = self.out_dir.join(MANIFEST_FILE);
        let mut body = internal_names.join("\n");
        body.push('\n');
        std::fs::write(&manifest, body)?;
        println!(
            "cargo:rustc-env=SUBCRATE_MANIFEST_DIR={}",
            self.out_dir.display()
        );

        Ok(())
    }

    fn compile_subcrate(&self, sub: &Subcrate, out_dir: &Path) -> Result<(), Error> {
        let args = self.rustc_args(sub);
        let source = render_source(&sub.inner_attrs, &sub.items);

        // Reuse decision: the crate *name* already captures the body as written
        // (a content hash), and the stamp additionally records the exact `rustc`
        // configuration *and the fully rendered source* — the latter so that
        // editing a file inlined via `mod foo;` (whose content is not in the
        // name) still forces a rebuild. We rebuild unless all of it matches. The
        // name itself stays config-independent so the proc macro can recompute it.
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
                sub.ident,
                if sub.proc_macro { " (proc-macro)" } else { "" },
                indent(stderr.trim_end())
            )));
        }

        std::fs::write(&stamp, fingerprint)?;
        Ok(())
    }

    /// Builds the `rustc` argument list (everything except the `-` input and
    /// `--out-dir`) used to compile `sub`. This is also hashed into the reuse
    /// stamp, so it must deterministically reflect the full configuration.
    fn rustc_args(&self, sub: &Subcrate) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();
        args.push("--edition".into());
        // A `#[subcrate(edition = "…")]` override wins over the package edition.
        args.push(sub.edition.clone().unwrap_or_else(|| self.edition.clone()));
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

        args.extend(self.extra_rustc_args.iter().cloned());
        args
    }
}

/// A discovered `#[subcrate]` module.
struct Subcrate {
    /// The module's identifier, e.g. `my_crate`.
    ident: String,
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
                ident,
                internal,
                proc_macro: args.proc_macro,
                edition: args.edition,
                inner_attrs: inner,
                items: compiled_items,
            });
        }
    }
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
/// attributes first (they must lead the crate root), then the items.
fn render_source(inner_attrs: &[Attribute], items: &[Item]) -> String {
    let tokens = quote! { #(#inner_attrs)* #(#items)* };
    tokens.to_string()
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
