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
//! * **`--target` for the host's own triple.** Cargo then keeps `RUSTFLAGS` away from
//!   build scripts and proc-macros, so a nested build must be told the target too.
//! * **Registry packages** (`mod registry`, on Unix): packages "published" to a
//!   directory source and built from a `CARGO_HOME` of their own that holds nothing
//!   else, so what the nested build sees is what the build being tested gives it: the
//!   dev-dependencies of the published package, the workspace's `[patch]` tables and
//!   Cargo configuration, and a git dependency.
//!
//! Each package is written into `CARGO_TARGET_TMPDIR` and built into one shared
//! target directory (the helper itself is built once). A cross-compiling test is
//! skipped, with a note, when its target is not installed.

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

/// A `cargo` command for the tests: what Cargo sets for the build that runs this test
/// must not leak into the one under test.
fn cargo_command(subcommand: &str) -> Command {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.arg(subcommand)
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CARGO_PKG_NAME")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env("CARGO_PROFILE_DEV_DEBUG", "0");
    cmd
}

/// Runs `cargo build` with `args` in `dir`.
fn cargo_build(dir: &Path, offline: bool, args: &[&str]) -> Output {
    cargo_build_env(dir, offline, args, &[])
}

/// Runs `cargo build` with `args` in `dir`, with the environment variables `envs`.
fn cargo_build_env(dir: &Path, offline: bool, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = cargo_command("build");
    cmd.args(args)
        .arg("--target-dir")
        .arg(shared_target())
        .current_dir(dir)
        .envs(envs.iter().cloned());
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

fn rustc() -> Command {
    Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
}

/// Whether `rustc` has the standard library of `target`.
fn target_installed(target: &str) -> bool {
    rustc()
        .args(["--print", "target-libdir", "--target", target])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map_or(false, |dir| Path::new(dir.trim()).is_dir())
}

/// The triple `rustc` builds for by default.
fn host_triple() -> String {
    let output = rustc().arg("-vV").output().expect("failed to run rustc");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .expect("no host in `rustc -vV`")
        .to_string()
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

/// The package of the cross-compiling tests, as `nested-cross-<name>` in a directory of
/// its own.
fn cross_package(name: &str) -> PathBuf {
    let dir = fresh(&format!("cross-{}", name));
    let manifest = CROSS_MANIFEST.replace("nested-cross", &format!("nested-cross-{}", name));
    write(&dir, "Cargo.toml", &manifest);
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
/// is in the debug profile directory `profile`.
fn host_build_in(profile: &Path, package: &str) -> bool {
    let dir = profile.join("subcrate-owner-deps").join(package);
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().ends_with("-host"))
}

/// Whether there is one in the profile directory of `target` under the shared target
/// directory.
fn has_host_build(target: &str, package: &str) -> bool {
    host_build_in(&shared_target().join(target).join("debug"), package)
}

/// The targets the cross-compiling test tries, when they are installed: bare metal (no
/// `std`, and `syn` could not be built for it at all), and the first of the targets with
/// an operating system that is (a library needs no linker, but every crate the package
/// uses is built for it, which takes its time: all of them with
/// `SUBCRATE_TEST_TARGETS=a,b,c`, which replaces this list).
fn cross_targets() -> Vec<String> {
    if let Ok(list) = std::env::var("SUBCRATE_TEST_TARGETS") {
        return list
            .split(',')
            .filter(|target| !target.is_empty())
            .map(str::to_string)
            .collect();
    }
    let with_os = [
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "wasm32-unknown-unknown",
    ];
    let mut targets = vec!["x86_64-unknown-none".to_string()];
    targets.extend(
        with_os
            .iter()
            .find(|target| target_installed(target))
            .map(|target| target.to_string()),
    );
    targets
}

#[test]
fn a_proc_macro_subcrate_with_owner_dependencies_cross_compiles() {
    let mut tried = 0;
    for target in &cross_targets() {
        if !target_installed(target) {
            eprintln!(
                "skipped: the `{}` target is not installed (`rustup target add {}`)",
                target, target
            );
            continue;
        }
        tried += 1;
        let dir = cross_package("targets");
        let offline = lock_like_the_workspace(&dir);

        // Natively first, as the baseline: one build serves both kinds of subcrate.
        assert_built(&cargo_build(&dir, offline, &[]));

        // For the target the proc-macro's crates are built for the host (it runs there)
        // — `syn` could not be built for a bare-metal target at all — and the library
        // subcrate's `shared` for the target. The `const` assertions in `src/lib.rs`
        // hold only if the macro ran and found both.
        let output = cargo_build(&dir, offline, &["--target", target.as_str()]);
        assert!(
            output.status.success(),
            "cargo --target {} failed:\n{}",
            target,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            has_host_build(target, "nested-cross-targets"),
            "no nested build for the host under {}",
            shared_target().join(target).display()
        );
    }
    if tried == 0 {
        eprintln!("skipped: none of the cross-compiling targets is installed");
    }
}

#[test]
fn a_target_that_is_the_hosts_own_triple_still_keeps_its_flags_from_the_host_builds() {
    // `--target` for the host's triple is not the same as no `--target`: Cargo then
    // applies `RUSTFLAGS` to the target's artifacts only, never to a build script or a
    // proc-macro. The flag here makes anything that is linked fail, which a library for
    // the target is not, and the `-C linker` of a target is a thing that is for it
    // alone. So the nested builds of the crates the subcrates use must be told the
    // target too — the library subcrates' with the flags, the proc-macro subcrate's for
    // the host without them — and the proc-macro subcrate itself, which the helper
    // compiles, takes none.
    let host = host_triple();
    let dir = cross_package("same-triple");
    let offline = lock_like_the_workspace(&dir);
    // For the host's own triple a proc-macro subcrate may also use a library subcrate,
    // which is built for the target (and so loadable, the triples being one): not so for
    // another triple, where the build helper refuses.
    let lib = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
    write(
        &dir,
        "src/lib.rs",
        &format!(
            "{}\n#[subcrate_macros::subcrate(proc_macro, lib_sub)]\npub mod pm_uses_lib {{\n    \
             use proc_macro::TokenStream;\n\n    #[proc_macro]\n    \
             pub fn value(_: TokenStream) -> TokenStream {{\n        \
             format!(\"{{}}u32\", lib_sub::value()).parse().unwrap()\n    }}\n}}\n\n\
             pub const FROM_PM: u32 = pm_uses_lib::value!();\nconst _: () = assert!(FROM_PM == 43);\n",
            lib
        ),
    );
    let flags = [("RUSTFLAGS", "-C linker=subcrate-test-no-such-linker")];

    let output = cargo_build_env(&dir, offline, &["--target", &host], &flags);
    assert!(
        output.status.success(),
        "cargo --target {} failed:\n{}",
        host,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        has_host_build(&host, "nested-cross-same-triple"),
        "no nested build for the host under {}",
        shared_target().join(&host).display()
    );

    // Without `--target` Cargo applies the flags to everything, as the nested build
    // does without one (a library needs no linker, so any flags will do): one nested
    // build, and none for the host.
    let dir = cross_package("implicit");
    let offline = lock_like_the_workspace(&dir);
    let flags = [("RUSTFLAGS", "-C debuginfo=0")];
    assert_built(&cargo_build_env(&dir, offline, &[], &flags));
    let profile = shared_target().join("debug");
    assert!(profile
        .join("subcrate-owner-deps/nested-cross-implicit")
        .is_dir());
    assert!(!host_build_in(&profile, "nested-cross-implicit"));
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

const SELF_USE_MANIFEST: &str = r#"
[package]
name = "nested-packaged-self"
version = "0.1.0"
edition = "2021"

[dependencies]
subcrate = { path = '{SUBCRATE}' }

[build-dependencies]
subcrate = { path = '{SUBCRATE}' }
"#;

#[test]
fn a_packaged_crate_is_the_package_its_own_build_script_takes_the_nested_build_for() {
    // A subcrate in `tests/` that uses the package's own library (`::nested_packaged_self`):
    // the package is built in the nested build too, and its build script, re-entered
    // there, must know that build is its own (`SUBCRATE_NESTED`) — or it would start a
    // nested build of its own, and so on. The nested build runs in the private
    // workspace, where the package is a symbolic link, so it is the link that Cargo
    // names as the package's directory, not the directory itself.
    let dir = fresh("packaged-self");
    let package = packaged_package(
        &dir,
        "nested-packaged-self-0.1.0",
        SELF_USE_MANIFEST,
        "",
        "pub fn text() -> &'static str {\n    \"owner\"\n}\n",
    );
    write(
        &package,
        "tests/uses_owner.rs",
        r#"
use subcrate::subcrate;

#[subcrate(::nested_packaged_self)]
mod uses_owner {
    pub fn text() -> &'static str {
        nested_packaged_self::text()
    }
}
"#,
    );
    let down = dir.join("down");
    write(
        &down,
        "Cargo.toml",
        &format!(
            "[package]\nname = \"down\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n\
             [dependencies]\nnested-packaged-self = {{ path = '{}' }}\n",
            package.display()
        ),
    );
    write(
        &down,
        "src/lib.rs",
        "pub fn text() -> &'static str { nested_packaged_self::text() }\n",
    );
    let offline = lock_like_the_workspace(&down);

    let output = cargo_build(&down, offline, &[]);
    assert_built(&output);
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

// -- Registry packages ------------------------------------------------------------

/// Packages "published" to a directory source (what `cargo vendor` makes, and a source
/// replacement of `crates-io` points to), built from a `CARGO_HOME` that holds nothing
/// else: Cargo then has no index and no cache to fall back to, so a nested build finds
/// a crate only where the build it is part of says it is. That is the "fresh cache"
/// case — which is where a registry package's nested build can only fail for want of
/// something it was not given, the warm cache of the developer's own machine hiding it.
///
/// Each downstream workspace keeps its target directory under it (a link to the shared
/// one), as it is in an ordinary build, for the build helper finds the workspace being
/// built above `OUT_DIR`. Needs symbolic links.
#[cfg(unix)]
mod registry {
    use super::*;
    use std::sync::Once;

    /// The version of the `subcrate` crates under test.
    fn subcrate_version() -> String {
        let manifest = std::fs::read_to_string(subcrate_dir().join("Cargo.toml")).unwrap();
        manifest
            .lines()
            .find_map(|line| line.strip_prefix("version = \""))
            .and_then(|version| version.strip_suffix('"'))
            .expect("no version in Cargo.toml")
            .to_string()
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    /// Puts a package into the directory source `registry`, as a registry has it: with
    /// the original manifest, `Cargo.toml.orig`, and what a directory source wants
    /// to check it by.
    fn publish(registry: &Path, name: &str, version: &str, manifest: &str, files: &[(&str, &str)]) {
        let dir = registry.join(format!("{}-{}", name, version));
        let manifest = manifest
            .replace("{VERSION}", &subcrate_version())
            .replace("{SUBCRATE}", &subcrate_dir().display().to_string());
        write(&dir, "Cargo.toml", &manifest);
        write(&dir, "Cargo.toml.orig", &manifest);
        for (path, text) in files {
            write(&dir, path, text);
        }
        write(
            &dir,
            ".cargo-checksum.json",
            &format!("{{\"files\":{{}},\"package\":\"{:0>64}\"}}", name.len()),
        );
    }

    /// A manifest as `cargo package` would leave it: nothing that needs a workspace, and
    /// the dependencies on the other `subcrate` crates as versions.
    fn normalized(manifest: &str) -> String {
        let end = manifest.find("\n[workspace]").unwrap_or(manifest.len());
        manifest[..end]
            .replace("path = \"macro\", ", "")
            .replace("path = \"core\", ", "")
            .replace("path = \"../core\", ", "")
    }

    /// The directory source every test starts from: the registry crates the `subcrate`
    /// crates are built from (what `cargo vendor` takes from the local cache, offline)
    /// and the `subcrate` crates themselves, as published.
    fn base_registry() -> PathBuf {
        static ONCE: Once = Once::new();
        let base = scratch("registry-base");
        ONCE.call_once(|| {
            let _ = std::fs::remove_dir_all(&base);
            let src = base.join("src");
            write(
                &src,
                "Cargo.toml",
                "[package]\nname = \"vendor-src\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
                 [workspace]\n\n[dependencies]\nsubcrate = { path = '{SUBCRATE}' }\n",
            );
            write(&src, "src/lib.rs", "");
            lock_like_the_workspace(&src);
            let registry = base.join("registry");
            let output = cargo_command("vendor")
                .args(["--offline"])
                .arg(&registry)
                .current_dir(&src)
                .output()
                .expect("failed to run cargo vendor");
            assert_built(&output);
            let version = subcrate_version();
            for (dir, name, files) in [
                (
                    "",
                    "subcrate",
                    &["lib.rs", "build.rs", "README.md", "LICENSE"][..],
                ),
                ("core", "subcrate-core", &["lib.rs"]),
                ("macro", "subcrate-macros", &["lib.rs"]),
            ] {
                let from = subcrate_dir().join(dir);
                let manifest = std::fs::read_to_string(from.join("Cargo.toml")).unwrap();
                let files: Vec<(String, String)> = files
                    .iter()
                    .map(|file| {
                        let text = std::fs::read_to_string(from.join(file)).unwrap();
                        (file.to_string(), text)
                    })
                    .collect();
                let files: Vec<(&str, &str)> = files
                    .iter()
                    .map(|(file, text)| (file.as_str(), text.as_str()))
                    .collect();
                publish(&registry, name, &version, &normalized(&manifest), &files);
            }
        });
        base.join("registry")
    }

    /// A directory source of its own for the test in `dir`: the base one, which the test
    /// adds its packages to.
    fn registry_in(dir: &Path) -> PathBuf {
        let registry = dir.join("registry");
        copy_dir(&base_registry(), &registry);
        registry
    }

    /// The Cargo configuration that replaces `crates-io` by the directory source.
    fn source_replacement(registry: &Path) -> String {
        format!(
            "[source.crates-io]\nreplace-with = \"test-registry\"\n\n\
             [source.test-registry]\ndirectory = '{}'\n",
            registry.display()
        )
    }

    /// A `CARGO_HOME` of its own, with `config` as its configuration file.
    fn cargo_home(dir: &Path, config: &str) -> PathBuf {
        let home = dir.join("cargo-home");
        write(&home, "config.toml", config);
        home
    }

    /// A manifest of a published package: `subcrate` as a dependency and a
    /// build-dependency, `quote` (which a subcrate uses), and what else `extra` says.
    fn package_manifest(name: &str, extra: &str) -> String {
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [dependencies]\nsubcrate = \"{{VERSION}}\"\nquote = \"1\"\n\n\
             [build-dependencies]\nsubcrate = \"{{VERSION}}\"\n{}",
            name, extra
        )
    }

    const BUILD_RS: &str = "fn main() { subcrate::build::compile().unwrap(); }\n";

    /// The library of a published package: a library subcrate on `quote`, which is
    /// what the nested build is for.
    const QUOTE_LIB: &str = r#"
use subcrate::subcrate;

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

    /// A downstream workspace `dir/down` with the given dependencies (and what else
    /// `extra` adds to the manifest) and a library that is `lib`, whose target directory
    /// is the shared one.
    fn downstream(dir: &Path, dependencies: &str, extra: &str, lib: &str) -> PathBuf {
        let down = dir.join("down");
        write(
            &down,
            "Cargo.toml",
            &format!(
                "[package]\nname = \"down\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                 [workspace]\n\n[dependencies]\n{}\n{}",
                dependencies, extra
            ),
        );
        write(&down, "src/lib.rs", lib);
        std::fs::create_dir_all(shared_target()).unwrap();
        std::os::unix::fs::symlink(shared_target(), down.join("target")).unwrap();
        down
    }

    /// `cargo build --offline` in `down`, with `home` as its `CARGO_HOME`.
    fn build_down(down: &Path, home: &Path) -> Output {
        cargo_command("build")
            .arg("--offline")
            .current_dir(down)
            .env("CARGO_HOME", home)
            .output()
            .expect("failed to run cargo")
    }

    #[test]
    fn a_registry_crates_dev_dependencies_are_not_resolved() {
        // The published package has a dev-dependency that the registry does not hold,
        // and that the workspace being built never fetched: Cargo does not resolve the
        // dev-dependencies of a dependency, and neither may the nested build.
        let dir = fresh("registry-devdep");
        let registry = registry_in(&dir);
        let manifest = package_manifest(
            "nested-devdep",
            "\n[dev-dependencies]\nnested-no-such-crate = \"1\"\n",
        );
        publish(
            &registry,
            "nested-devdep",
            "0.1.0",
            &manifest,
            &[("build.rs", BUILD_RS), ("src/lib.rs", QUOTE_LIB)],
        );
        let home = cargo_home(&dir, &source_replacement(&registry));
        let down = downstream(
            &dir,
            "nested-devdep = \"0.1\"",
            "",
            "pub fn text() -> String { nested_devdep::text() }\n",
        );
        assert_built(&build_down(&down, &home));
    }

    /// The library of a package that has a subcrate on `nested-patched-dep`, a crate
    /// that comes in two kinds: the registry's and the patched one.
    const WHO_LIB: &str = r#"
use subcrate::subcrate;

#[subcrate(::nested_patched_dep)]
pub mod uses_dep {
    pub const WHO: &str = nested_patched_dep::WHO;
}

pub const WHO: &str = uses_dep::WHO;
"#;

    /// A downstream workspace that builds `nested-patched`, which depends on
    /// `nested-patched-dep`, a crate that is published as `registry` and is `patched`
    /// in the workspace by the `overrides` (`[patch]` or `[replace]` tables). The build
    /// only succeeds if the subcrate links the patched one, as the package does.
    fn build_patched(test: &str, overrides: &str, patched_version: &str) {
        let dir = fresh(test);
        let registry = registry_in(&dir);
        let dep_lib = |who: &str| format!("pub const WHO: &str = \"{}\";\n", who);
        let dep_manifest = |version: &str| {
            format!(
                "[package]\nname = \"nested-patched-dep\"\nversion = \"{}\"\nedition = \"2021\"\n",
                version
            )
        };
        publish(
            &registry,
            "nested-patched-dep",
            "1.0.0",
            &dep_manifest("1.0.0"),
            &[("src/lib.rs", &dep_lib("registry"))],
        );
        let manifest = package_manifest("nested-patched", "").replace(
            "[dependencies]\n",
            "[dependencies]\nnested-patched-dep = \"1\"\n",
        );
        publish(
            &registry,
            "nested-patched",
            "0.1.0",
            &manifest,
            &[("build.rs", BUILD_RS), ("src/lib.rs", WHO_LIB)],
        );
        let home = cargo_home(&dir, &source_replacement(&registry));
        let down = downstream(
            &dir,
            "nested-patched = \"0.1\"",
            overrides,
            "const fn same(a: &str, b: &str) -> bool {\n    let (a, b) = (a.as_bytes(), b.as_bytes());\n    \
             if a.len() != b.len() {\n        return false;\n    }\n    let mut i = 0;\n    \
             while i < a.len() {\n        if a[i] != b[i] {\n            return false;\n        }\n        \
             i += 1;\n    }\n    true\n}\n\
             // The package itself uses the patched crate; so must what its subcrate links.\n\
             const _: () = assert!(same(nested_patched::WHO, \"patched\"));\n",
        );
        // What the patch points to, relative to the workspace's manifest.
        write(
            &down,
            "patched/nested-patched-dep/Cargo.toml",
            &dep_manifest(patched_version),
        );
        write(
            &down,
            "patched/nested-patched-dep/src/lib.rs",
            &dep_lib("patched"),
        );
        assert_built(&build_down(&down, &home));
    }

    #[test]
    fn the_workspaces_patch_tables_reach_the_nested_build_of_a_registry_crate() {
        // Without it the subcrate links the registry's `nested-patched-dep` (a warm
        // cache has it) or finds none (a fresh one), while the package uses the patched
        // one. The path is relative to the workspace's manifest.
        build_patched(
            "registry-patch",
            "\n[patch.crates-io]\nnested-patched-dep = { path = \"patched/nested-patched-dep\" }\n",
            "1.0.1",
        );
    }

    #[test]
    fn a_crate_the_workspace_replaces_is_ambiguous_to_the_nested_build_which_says_so() {
        // `[replace]` is carried over too (the nested build resolves the same graph), but
        // the crate it swaps is two packages of one name and version, and `cargo build -p`
        // takes neither: the error says to use `[patch]`, rather than the cause being a
        // mystery. (Nothing in the nested build would otherwise mind the replacement.)
        let result = std::panic::catch_unwind(|| {
            build_patched(
                "registry-replace",
                "\n[replace]\n\"nested-patched-dep:1.0.0\" = { path = \"patched/nested-patched-dep\" }\n",
                "1.0.0",
            )
        });
        let message = *result
            .expect_err("a replaced crate was built")
            .downcast::<String>()
            .unwrap();
        assert!(message.contains("is ambiguous"), "{}", message);
        assert!(message.contains("use `[patch]` instead"), "{}", message);
    }

    #[test]
    fn the_workspaces_cargo_configuration_reaches_the_nested_build_of_a_registry_crate() {
        // The directory source is replaced in the workspace's `.cargo/config.toml`, and
        // `CARGO_HOME` has none: a nested build run from the package's own directory,
        // in the registry's, would not see it, and find no crate at all.
        let dir = fresh("registry-config");
        let registry = registry_in(&dir);
        publish(
            &registry,
            "nested-config",
            "0.1.0",
            &package_manifest("nested-config", ""),
            &[("build.rs", BUILD_RS), ("src/lib.rs", QUOTE_LIB)],
        );
        let home = cargo_home(&dir, "");
        let down = downstream(
            &dir,
            "nested-config = \"0.1\"",
            "",
            "pub fn text() -> String { nested_config::text() }\n",
        );
        write(&down, ".cargo/config.toml", &source_replacement(&registry));
        assert_built(&build_down(&down, &home));
    }

    #[test]
    fn a_git_dependency_is_built_without_writing_into_its_checkout() {
        // A member of a workspace in a git repository: its manifest inherits from the
        // workspace and depends on a sibling by a relative path, so it cannot stand
        // alone where it is. The nested build must neither write the `Cargo.lock` of
        // that workspace into the checkout in `CARGO_HOME` (which every project that
        // uses that revision shares), nor resolve it afresh.
        let git = |dir: &Path, args: &[&str]| {
            Command::new("git")
                .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
                .args(args)
                .current_dir(dir)
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false)
        };
        let dir = fresh("registry-git");
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        if !git(&repo, &["init", "-q"]) {
            eprintln!("skipped: `git` is not available");
            return;
        }
        let version = subcrate_version();
        write(
            &repo,
            "Cargo.toml",
            "[workspace]\nmembers = [\"pkg\", \"sibling\"]\nresolver = \"2\"\n\n\
             [workspace.package]\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [workspace.dependencies]\nquote = \"1\"\n",
        );
        write(
            &repo,
            "pkg/Cargo.toml",
            &format!(
                "[package]\nname = \"nested-git\"\nversion.workspace = true\n\
                 edition.workspace = true\n\n[dependencies]\nsubcrate = \"{0}\"\n\
                 quote.workspace = true\nnested-git-sibling = {{ path = \"../sibling\", \
                 version = \"0.1\" }}\n\n[build-dependencies]\nsubcrate = \"{0}\"\n",
                version
            ),
        );
        write(&repo, "pkg/build.rs", BUILD_RS);
        write(
            &repo,
            "pkg/src/lib.rs",
            // A subcrate on `quote` and one on the sibling, a path dependency (which
            // is also a member of the repository's workspace).
            &format!(
                "{}\n#[subcrate(::nested_git_sibling)]\npub mod uses_sibling {{\n    \
                 pub fn value() -> u32 {{\n        nested_git_sibling::value()\n    }}\n}}\n",
                QUOTE_LIB
            ),
        );
        write(
            &repo,
            "sibling/Cargo.toml",
            "[package]\nname = \"nested-git-sibling\"\nversion.workspace = true\n\
             edition.workspace = true\n",
        );
        write(
            &repo,
            "sibling/src/lib.rs",
            "pub fn value() -> u32 {\n    1\n}\n",
        );
        assert!(git(&repo, &["add", "."]) && git(&repo, &["commit", "-q", "-m", "test"]));

        let registry = registry_in(&dir);
        let home = cargo_home(&dir, &source_replacement(&registry));
        let down = downstream(
            &dir,
            &format!("nested-git = {{ git = \"file://{}\" }}", repo.display()),
            "",
            "pub fn text() -> String { nested_git::text() }\n\
             pub fn value() -> u32 { nested_git::uses_sibling::value() }\n",
        );
        // Not `--offline`: the repository is fetched (from a local path, so no network).
        let output = cargo_command("build")
            .current_dir(&down)
            .env("CARGO_HOME", &home)
            .output()
            .expect("failed to run cargo");
        assert_built(&output);

        let mut written = Vec::new();
        let checkouts = home.join("git/checkouts");
        for repo in std::fs::read_dir(&checkouts).unwrap().flatten() {
            for revision in std::fs::read_dir(repo.path()).unwrap().flatten() {
                if revision.path().join("Cargo.lock").exists() {
                    written.push(revision.path());
                }
            }
        }
        assert!(written.is_empty(), "Cargo.lock written into {:?}", written);
    }
    /// Builds a downstream workspace whose registry package `name` has a subcrate on
    /// `probe-dep`, a crate the registry holds, which `change` (of the package's
    /// manifest) declares in the way a case is about, and returns the error of the
    /// build, which has to fail.
    fn failing_build(name: &str, change: &dyn Fn(String) -> String) -> String {
        let dir = fresh(&format!("registry-{}", name));
        let registry = registry_in(&dir);
        publish(
            &registry,
            "probe-dep",
            "1.0.0",
            "[package]\nname = \"probe-dep\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
            &[("src/lib.rs", "pub const WHO: &str = \"registry\";\n")],
        );
        let lib = WHO_LIB.replace("nested_patched_dep", "probe_dep");
        publish(
            &registry,
            name,
            "0.1.0",
            &change(package_manifest(name, "")),
            &[("build.rs", BUILD_RS), ("src/lib.rs", &lib)],
        );
        let home = cargo_home(&dir, &source_replacement(&registry));
        let down = downstream(
            &dir,
            &format!(
                "{} = {{ version = \"0.1\", default-features = false }}",
                name
            ),
            "",
            "",
        );
        let output = build_down(&down, &home);
        assert!(
            !output.status.success(),
            "the build of `{}` succeeded",
            name
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    #[test]
    fn a_dependency_the_resolution_does_not_switch_on_is_reported_for_what_it_is() {
        // The nested build has the features the outer build resolved, as for a package in
        // the workspace, so an optional dependency that is off is none of its graph: Cargo
        // says no package of that name is there, and what to check is the declaration.
        let message = failing_build("nested-optional", &|manifest| {
            manifest
                .replace(
                    "[dependencies]\n",
                    "[dependencies]\nprobe-dep = { version = \"1\", optional = true }\n",
                )
                .replace(
                    "\n[build-dependencies]",
                    "\n[features]\ndefault = [\"probe-dep\"]\n\n[build-dependencies]",
                )
        });
        assert!(
            message.contains("did not match any packages"),
            "{}",
            message
        );
        assert!(
            message.contains("Every `::name` a `#[subcrate]` uses must be a dependency"),
            "{}",
            message
        );

        // A build-dependency that is not a normal one is in the graph, but Cargo cannot
        // build it for the build's platform and stops, which is another thing to say.
        let message = failing_build("nested-build-only", &|manifest| {
            format!("{}probe-dep = \"1\"\n", manifest)
        });
        assert!(
            message.contains("failed to build the crates the subcrates depend on"),
            "{}",
            message
        );
        assert!(
            !message.contains("Every `::name` a `#[subcrate]` uses must be a dependency"),
            "{}",
            message
        );
        if message.contains("features.rs") {
            assert!(
                message.contains("has to be a normal dependency"),
                "{}",
                message
            );
        }
    }
}
