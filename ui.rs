//! Test-time harness for a `#[subcrate(should_error …)]` module in a package with
//! no build script.
//!
//! When no `build.rs` ran [`subcrate::build::compile`](crate::build::compile), a
//! `should_error` subcrate — which is never linked — expands (see `subcrate-macros`)
//! to a `#[test]` that calls [`run`] with a [`Case`] carrying the module body and its
//! expectations, instead of needing the build helper. At test time [`run`] compiles
//! that body as a throwaway crate with `rustc` — the source on stdin,
//! `--emit=metadata` (no codegen), diagnostics as JSON — and asserts them with the
//! same checker the build helper uses.
//!
//! Because the work happens in a `#[test]`, not a build script, a compile-fail suite
//! needs only `subcrate` under `[dev-dependencies]`: no `build.rs`, nothing in a
//! consumer's dependency graph, and each case is reported as its own test.
//!
//! ## What a case can depend on
//!
//! A `#[subcrate(::some_crate, should_error)]` dependency is resolved to the
//! already-built `libsome_crate-*.rlib` among the running test crate's own build
//! artifacts. That means a case can link **any crate the test crate itself links**,
//! including a `[dev-dependencies]` one — the subcrate's dependency only has to be a
//! dependency of the test crate too. Sibling-subcrate dependencies (a path to
//! another `#[subcrate]` module) and `proc_macro` subcrates need the build script and
//! are rejected on this path.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// One UI-test case, embedded by the `#[subcrate]` expansion and run by [`run`].
///
/// Every field is `'static` so the whole value is a compile-time constant the
/// macro writes; nothing here is computed at run time except by [`run`].
pub struct Case {
    /// The module's identifier, used as the `#[test]` name and in messages.
    pub name: &'static str,
    /// The file the case was written in (`file!()`), for the failure message.
    pub source_file: &'static str,
    /// The crate-root source compiled for the case: the module's crate-level inner
    /// attributes followed by its items, as written.
    pub body: &'static str,
    /// The external crates the case links, each resolved against the test binary's
    /// `deps` directory (see the module docs).
    pub deps: &'static [Dep],
    /// The edition to compile with (`#[subcrate(edition = "…")]`); `None` uses
    /// [`DEFAULT_EDITION`].
    pub edition: Option<&'static str>,
    /// The `should_*` expectations the diagnostics must satisfy.
    pub expect: Expect,
}

/// One external dependency of a [`Case`].
pub struct Dep {
    /// The name the case's body refers to the crate by (`--extern <alias>=…`): the
    /// `alias = ` if given, else the crate name.
    pub alias: &'static str,
    /// The crate's own name, snake-cased as it appears in `lib<krate>-<hash>.rlib`.
    pub krate: &'static str,
}

/// A [`Case`]'s UI-test expectations, mirroring
/// [`subcrate_core::UiExpectations`](https://docs.rs/subcrate-core). `Some(s)`
/// requires the substring `s`; a bare entry is `None`.
pub struct Expect {
    /// `should_error`: the case must fail to compile (and, for each `Some`, an
    /// error must contain that text).
    pub should_errors: &'static [Option<&'static str>],
    /// `should_warn`: a warning must be emitted (and contain each `Some`'s text).
    pub should_warns: &'static [Option<&'static str>],
    /// `should_spanned`: a diagnostic must underline code equal to each string
    /// (whitespace ignored).
    pub should_spanned: &'static [&'static str],
}

/// The edition a case is compiled with when it sets no `edition = "…"`.
pub const DEFAULT_EDITION: &str = "2021";

/// Runs a UI-test [`Case`]: compiles its body and asserts its diagnostics. Panics
/// (failing the enclosing `#[test]`) with a readable report if an expectation is
/// unmet or the compile could not be run.
pub fn run(case: &Case) {
    if let Err(message) = try_run(case) {
        panic!(
            "subcrate UI test `{}` ({}): {}",
            case.name, case.source_file, message
        );
    }
}

fn try_run(case: &Case) -> Result<(), String> {
    let (rlibs, search_dirs) = artifacts()?;
    let mut externs = Vec::with_capacity(case.deps.len());
    for dep in case.deps {
        let rlib = rlibs.get(dep.krate).ok_or_else(|| {
            format!(
                "could not find a compiled `{krate}` among this test crate's build artifacts. \
                 A `#[subcrate(::{krate}, should_error)]` dependency has to be a dependency of this \
                 crate too — add `{krate}` under `[dependencies]` or `[dev-dependencies]`, and \
                 refer to it by its real crate name (a Cargo rename is not resolved here).",
                krate = dep.krate,
            )
        })?;
        // `--emit=metadata` resolves dependencies by their `.rmeta` (the `.rlib` may
        // be a pipelined metadata stub rustc rejects), as `cargo check` does; fall
        // back to the `.rlib` when no `.rmeta` sits beside it.
        let rmeta = rlib.with_extension("rmeta");
        let artifact = if rmeta.is_file() { rmeta } else { rlib.clone() };
        externs.push(format!("{}={}", dep.alias, artifact.display()));
    }

    let out_dir = scratch_dir(case.name)?;
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    let mut cmd = Command::new(&rustc);
    cmd.arg("-") // read the crate root from stdin
        .arg("--edition")
        .arg(case.edition.unwrap_or(DEFAULT_EDITION))
        .arg("--crate-type")
        .arg("lib")
        .arg("--crate-name")
        .arg("subcrate_check_case")
        .arg("--error-format=json")
        .arg("--emit=metadata") // type-check only: errors and warnings, no codegen
        .arg("--out-dir")
        .arg(&out_dir);
    for dir in &search_dirs {
        let mut l = OsString::from("dependency=");
        l.push(dir);
        cmd.arg("-L").arg(l);
    }
    for ext in &externs {
        cmd.arg("--extern").arg(ext);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to launch `{}`: {}", to_lossy(&rustc), e))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(case.body.as_bytes())
            .map_err(|e| format!("failed to send the case source to rustc: {}", e))?;
        // Dropping `stdin` closes the pipe so rustc sees EOF.
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed to run rustc: {}", e))?;
    let _ = std::fs::remove_dir_all(&out_dir);

    let should_errors: Vec<Option<String>> = owned_opts(case.expect.should_errors);
    let should_warns: Vec<Option<String>> = owned_opts(case.expect.should_warns);
    let should_spanned: Vec<String> = case.expect.should_spanned.iter().map(|s| s.to_string()).collect();

    crate::build::ui_check_from_json(
        output.status.success(),
        &String::from_utf8_lossy(&output.stderr),
        &should_errors,
        &should_warns,
        &should_spanned,
    )
}

/// The compiled crates available to a case and the directories to search for their
/// own dependencies (`-L dependency=…`), gathered from the running test crate's own
/// build output. Returns a map from crate name (as in `lib<crate>-<hash>.rlib`) to
/// its newest artifact.
///
/// It scans the `deps` and `build` directories under the profile root
/// (`target/<profile>`): a stable toolchain puts a crate's file in
/// `deps/lib<crate>-<hash>.rlib`, a recent nightly under `build/<pkg>/<hash>/out/`.
/// It deliberately avoids `subcrate-owner-deps/` (the build helper's private nested
/// target), so only the crates this test crate actually links are seen.
fn artifacts() -> Result<(BTreeMap<String, PathBuf>, BTreeSet<PathBuf>), String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("could not locate the test binary: {}", e))?;
    let exe_dir = exe
        .parent()
        .ok_or_else(|| "the test binary has no parent directory".to_string())?;
    let root = profile_root(exe_dir);

    let mut rlibs: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    for sub in ["deps", "build"] {
        let base = root.join(sub);
        if !base.is_dir() {
            continue;
        }
        let (found, found_dirs) = crate::build::scan_build_artifacts(&base);
        for (name, path) in found {
            rlibs.entry(name).or_insert(path);
        }
        dirs.extend(found_dirs);
    }
    Ok((rlibs, dirs))
}

/// Finds the profile root (`target/<profile>`) above the test binary: the parent of
/// the nearest ancestor named `deps` or `build`. The binary sits in `deps/` on a
/// stable toolchain and in `build/<pkg>/<hash>/out/` on a recent nightly (which may
/// have no `deps/` directory at all), so keying on either covers both.
fn profile_root(exe_dir: &Path) -> PathBuf {
    let mut cur = Some(exe_dir);
    while let Some(dir) = cur {
        if matches!(
            dir.file_name().and_then(|n| n.to_str()),
            Some("deps") | Some("build")
        ) {
            if let Some(parent) = dir.parent() {
                return parent.to_path_buf();
            }
        }
        cur = dir.parent();
    }
    exe_dir.to_path_buf()
}

/// A unique, writable scratch directory for one case's `--emit=metadata` output.
fn scratch_dir(name: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!(
        "subcrate-ui-{}-{}",
        std::process::id(),
        sanitize(name)
    ));
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create a scratch directory ({}): {}", dir.display(), e))?;
    Ok(dir)
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn owned_opts(opts: &[Option<&str>]) -> Vec<Option<String>> {
    opts.iter().map(|o| o.map(|s| s.to_string())).collect()
}

fn to_lossy(s: &OsString) -> String {
    s.to_string_lossy().into_owned()
}
