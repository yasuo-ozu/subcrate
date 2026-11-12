//! The nested `cargo build` that the build helper runs for the crates a subcrate
//! takes from its package (`#[subcrate(::dep)]`), exercised on small packages built
//! by a real `cargo`:
//!
//! * **Cross-compiling.** A proc-macro subcrate runs on the host, so the crates it
//!   uses are built for the host (a nested build of their own), next to the target's
//!   build that serves the library subcrates. A `#![no_std]` library with such a
//!   proc-macro builds for `x86_64-unknown-none`.
//! * **A packaged crate's own `Cargo.lock`.** For a crate built from a registry the
//!   nested build runs in a private workspace of its own, so it neither uses the
//!   `Cargo.lock` the crate was published with (which can name versions the local
//!   cache does not hold) nor rewrites it in the registry's source directory.
//! * **`cargo package`'s verification of several packages**, whose dependencies on
//!   each other are met by Cargo's local overlay registry, which a nested `cargo`
//!   cannot see: the build helper points to the extracted siblings instead.
//!
//! Each package is written into `CARGO_TARGET_TMPDIR` and built into one shared
//! target directory (the helper itself is built once). The cross-compiling test is
//! skipped, with a note, when the `x86_64-unknown-none` target is not installed.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The `subcrate` workspace: where the packages under test find `subcrate` and
/// `subcrate-macros` (by path, so the helper under test is the one built here).
fn subcrate_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("nested-build");
    dir.join(name)
}

/// The target directory every package here builds into.
fn shared_target() -> PathBuf {
    scratch("target")
}

/// Writes `text` to `dir/path`, with `{SUBCRATE}` replaced by the `subcrate` directory
/// (the packages are written in TOML, where a path is best a literal string: `'…'`).
fn write(dir: &Path, path: &str, text: &str) {
    let file = dir.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let text = text.replace("{SUBCRATE}", &subcrate_dir().display().to_string());
    std::fs::write(file, text).unwrap();
}

/// A fresh directory for the package `name`.
fn fresh(name: &str) -> PathBuf {
    let dir = scratch(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Gives the packages in `dir` the lock file of this workspace, so they resolve — and,
/// if it exists, build `--offline` — from the versions already in the local cache.
fn lock_like_the_workspace(dir: &Path) -> bool {
    let lock = subcrate_dir().join("Cargo.lock");
    std::fs::copy(lock, dir.join("Cargo.lock")).is_ok()
}

/// Runs `cargo build` with `args` in `dir`.
fn cargo_build(dir: &Path, offline: bool, args: &[&str]) -> Output {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.arg("build")
        .args(args)
        .arg("--target-dir")
        .arg(shared_target())
        .current_dir(dir)
        // What Cargo sets for the build that runs this test must not leak into the
        // one under test.
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CARGO_PKG_NAME")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env("CARGO_PROFILE_DEV_DEBUG", "0");
    if offline {
        cmd.arg("--offline");
    }
    cmd.output().expect("failed to run cargo")
}

fn assert_built(output: &Output) {
    assert!(
        output.status.success(),
        "cargo failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Whether `rustc` has the standard library of `target`.
fn target_installed(target: &str) -> bool {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    Command::new(rustc)
        .args(["--print", "target-libdir", "--target", target])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map_or(false, |dir| Path::new(dir.trim()).is_dir())
}

// -- Cross-compiling -------------------------------------------------------------

const CROSS_MANIFEST: &str = r#"
[package]
name = "nested-cross"
version = "0.1.0"
edition = "2021"

# Its own workspace, apart from this one.
[workspace]

[dependencies]
# The attribute alone: `subcrate` itself is a `std` crate, which the bare-metal target
# does not have.
subcrate-macros = { path = '{SUBCRATE}/macro' }
# Builds for any target, and is used by both kinds of subcrate below.
shared = { path = "shared" }

# What a proc-macro subcrate uses must be a normal dependency that is switched on for
# the host, but cannot be built for the target: so it is left out of the bare-metal
# build by its platform. (An optional dependency whose feature is off, or one that is
# only a build-dependency, is not switched on for the nested build, and Cargo cannot
# build it.)
[target.'cfg(not(target_os = "none"))'.dependencies]
syn = { version = "2", features = ["full"] }

[build-dependencies]
subcrate = { path = '{SUBCRATE}' }
"#;

const CROSS_LIB: &str = r#"
#![no_std]

// Runs on the host, where it uses `syn` (a crate that cannot be built for the target)
// and `shared`, built for the host.
#[subcrate_macros::subcrate(proc_macro, ::syn, ::shared)]
pub mod pm {
    use proc_macro::TokenStream;

    #[proc_macro]
    pub fn items(input: TokenStream) -> TokenStream {
        let file: syn::File = syn::parse(input).unwrap();
        format!("{}u32", file.items.len() as u32 * 100 + shared::answer())
            .parse()
            .unwrap()
    }
}

// A library, built for the target, against the target's `shared`.
#[subcrate_macros::subcrate(::shared)]
pub mod lib_sub {
    #![no_std]
    pub const fn value() -> u32 {
        shared::answer() + 1
    }
}

// Checked by the compiler: the macro ran, and found both crates.
pub const ITEMS: u32 = pm::items!(fn a() {} fn b() {});
const _: () = assert!(ITEMS == 242);
pub const VALUE: u32 = lib_sub::value();
const _: () = assert!(VALUE == 43);
"#;

fn cross_package() -> PathBuf {
    let dir = fresh("cross");
    write(&dir, "Cargo.toml", CROSS_MANIFEST);
    write(
        &dir,
        "build.rs",
        "fn main() { subcrate::build::compile().unwrap(); }\n",
    );
    write(&dir, "src/lib.rs", CROSS_LIB);
    write(
        &dir,
        "shared/Cargo.toml",
        "[package]\nname = \"shared\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        &dir,
        "shared/src/lib.rs",
        "#![no_std]\npub const fn answer() -> u32 {\n    42\n}\n",
    );
    dir
}

/// Whether the nested build for the host (`…/subcrate-owner-deps/<package>/<compiler>-host`)
/// is in the profile directory of `target` under the shared target directory.
fn has_host_build(target: &str, package: &str) -> bool {
    let dir = shared_target()
        .join(target)
        .join("debug")
        .join("subcrate-owner-deps")
        .join(package);
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().ends_with("-host"))
}

#[test]
fn a_proc_macro_subcrate_with_owner_dependencies_cross_compiles() {
    let target = "x86_64-unknown-none";
    if !target_installed(target) {
        eprintln!(
            "skipped: the `{}` target is not installed (`rustup target add {}`)",
            target, target
        );
        return;
    }
    let dir = cross_package();
    let offline = lock_like_the_workspace(&dir);

    // Natively first, as the baseline: one build serves both kinds of subcrate.
    assert_built(&cargo_build(&dir, offline, &[]));

    // For the bare-metal target the proc-macro's crates are built for the host (it
    // runs there) — `syn` could not be built for the target at all — and the library
    // subcrate's `shared` for the target. The `const` assertions in `src/lib.rs` hold
    // only if the macro ran and found both.
    assert_built(&cargo_build(&dir, offline, &["--target", target]));
    assert!(
        has_host_build(target, "nested-cross"),
        "no nested build for the host under {}",
        shared_target().join(target).display()
    );
}

// -- A packaged crate's own lock file ---------------------------------------------

/// The lock file a crate is published with, naming a version of `quote` that no
/// registry cache holds. Used as it is, the nested `--offline` build could not get it.
const STALE_LOCK: &str = r#"# This file is automatically @generated by Cargo.
version = 3

[[package]]
name = "nested-packaged"
version = "0.1.0"
dependencies = [
 "quote",
 "subcrate",
]

[[package]]
name = "quote"
version = "1.0.9999"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"
"#;

const PACKAGED_MANIFEST: &str = r#"
[package]
name = "nested-packaged"
version = "0.1.0"
edition = "2021"

[dependencies]
subcrate = { path = '{SUBCRATE}' }
quote = "1"

[build-dependencies]
subcrate = { path = '{SUBCRATE}' }
"#;

const PACKAGED_LIB: &str = r#"
use subcrate::subcrate;

// A library subcrate on one of the package's dependencies.
#[subcrate(::quote)]
pub mod uses_quote {
    pub fn text() -> String {
        quote::quote!(a + b).to_string()
    }
}

pub fn text() -> String {
    uses_quote::text()
}
"#;

/// A package as a registry (or `cargo package`) leaves it: with `Cargo.toml.orig`.
fn packaged_package(dir: &Path, name: &str, manifest: &str, lock: &str, lib: &str) -> PathBuf {
    let package = dir.join(name);
    write(&package, "Cargo.toml", manifest);
    write(&package, "Cargo.toml.orig", manifest);
    write(&package, "Cargo.lock", lock);
    write(
        &package,
        "build.rs",
        "fn main() { subcrate::build::compile().unwrap(); }\n",
    );
    write(&package, "src/lib.rs", lib);
    package
}

#[test]
fn a_packaged_crates_own_lock_file_is_neither_used_nor_rewritten() {
    let dir = fresh("packaged");
    let package = packaged_package(
        &dir,
        "nested-packaged-0.1.0",
        PACKAGED_MANIFEST,
        STALE_LOCK,
        PACKAGED_LIB,
    );
    // The workspace being built depends on the package by path (a registry package
    // is not available here); what makes it "built from a registry" is the marker.
    let down = dir.join("down");
    write(
        &down,
        "Cargo.toml",
        &format!(
            "[package]\nname = \"down\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n\
             [dependencies]\nnested-packaged = {{ path = '{}' }}\n",
            package.display()
        ),
    );
    write(
        &down,
        "src/lib.rs",
        "pub fn text() -> String { nested_packaged::text() }\n",
    );
    let offline = lock_like_the_workspace(&down);

    let before = std::fs::read(package.join("Cargo.lock")).unwrap();
    let output = cargo_build(&down, offline, &[]);
    assert_built(&output);

    // The package's own lock file is as it was published, and nothing else was
    // written into its directory.
    assert_eq!(std::fs::read(package.join("Cargo.lock")).unwrap(), before);
    let mut files: Vec<String> = std::fs::read_dir(&package)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            "Cargo.lock",
            "Cargo.toml",
            "Cargo.toml.orig",
            "build.rs",
            "src"
        ]
    );
}

// -- `cargo package` verifying several packages ------------------------------------

const OVERLAY_B_MANIFEST: &str = r#"
[package]
name = "nested-overlay-b"
version = "0.1.0"
edition = "2021"
"#;

const OVERLAY_A_MANIFEST: &str = r#"
[package]
name = "nested-overlay-a"
version = "0.1.0"
edition = "2021"

[dependencies]
subcrate = { path = '{SUBCRATE}' }
nested-overlay-b = "0.1.0"

[build-dependencies]
subcrate = { path = '{SUBCRATE}' }
"#;

/// `cargo package`'s lock file for `a`: its sibling `b`, as a registry package that
/// has a checksum (of the file in Cargo's overlay, which no registry has).
const OVERLAY_A_LOCK: &str = r#"# This file is automatically @generated by Cargo.
version = 3

[[package]]
name = "nested-overlay-a"
version = "0.1.0"
dependencies = [
 "nested-overlay-b",
 "subcrate",
]

[[package]]
name = "nested-overlay-b"
version = "0.1.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "1111111111111111111111111111111111111111111111111111111111111111"
"#;

const OVERLAY_A_LIB: &str = r#"
use subcrate::subcrate;

#[subcrate(::nested_overlay_b)]
pub mod uses_b {
    pub fn value() -> u32 {
        nested_overlay_b::value() + 1
    }
}

pub fn value() -> u32 {
    uses_b::value()
}
"#;

#[test]
fn a_package_verified_against_its_siblings_finds_them_beyond_cargos_overlay() {
    // What `cargo package -p a -p b` leaves in `target/package`: each package extracted,
    // and `tmp-registry`, Cargo's overlay that holds the `.crate` files and that only
    // Cargo's own process sees. `a` depends on `b`, which no registry has. (The test
    // lays this out by hand: `cargo package` would need the helper under test
    // published too.)
    let dir = fresh("overlay");
    let package_dir = dir.join("target/package");
    std::fs::create_dir_all(package_dir.join("tmp-registry")).unwrap();
    let b = package_dir.join("nested-overlay-b-0.1.0");
    write(&b, "Cargo.toml", OVERLAY_B_MANIFEST);
    write(&b, "Cargo.toml.orig", OVERLAY_B_MANIFEST);
    write(&b, "src/lib.rs", "pub fn value() -> u32 {\n    41\n}\n");
    let a = packaged_package(
        &package_dir,
        "nested-overlay-a-0.1.0",
        OVERLAY_A_MANIFEST,
        OVERLAY_A_LOCK,
        OVERLAY_A_LIB,
    );

    // The outer build gets `b` by a patch, as Cargo's own does from the overlay.
    let down = dir.join("down");
    write(
        &down,
        "Cargo.toml",
        &format!(
            "[package]\nname = \"down\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n\
             [dependencies]\nnested-overlay-a = {{ path = '{}' }}\n\n\
             [patch.crates-io]\nnested-overlay-b = {{ path = '{}' }}\n",
            a.display(),
            b.display()
        ),
    );
    write(
        &down,
        "src/lib.rs",
        "pub fn value() -> u32 { nested_overlay_a::value() }\n",
    );
    let offline = lock_like_the_workspace(&down);

    let before = std::fs::read(a.join("Cargo.lock")).unwrap();
    assert_built(&cargo_build(&down, offline, &[]));
    assert_eq!(std::fs::read(a.join("Cargo.lock")).unwrap(), before);
}
