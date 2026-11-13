//! The `cfg(doc)` stand-in of a `proc_macro` subcrate: `cargo doc --no-deps` has to
//! document a package that re-exports a function-like, an attribute and a derive macro
//! from its proc-macro subcrate (`pub use macros::{my_func, my_attr, MyDerive};`).
//!
//! The fixture is the package `examples/doc-macros`. This test runs `cargo doc` on it in
//! its own target directory (so it does not wait for the lock of the one running the
//! tests) and looks at the pages. Rustdoc can show an attribute as `attr.NAME.html` and
//! a derive as `derive.NAME.html` only from a real proc-macro crate it can load, which
//! the stand-in cannot be, so all three come out as `macro.NAME.html` pages.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Every file under `dir`, as paths relative to `dir`.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.push(path.strip_prefix(root).unwrap().to_path_buf());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn cargo_doc_covers_function_like_attribute_and_derive_macros() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/doc-macros/Cargo.toml");
    let target_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("doc-macros");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    // Rustdoc warnings (a broken link, an unused stub, ...) are errors here.
    let rustdocflags = format!(
        "{} -D warnings",
        std::env::var("RUSTDOCFLAGS").unwrap_or_default()
    );
    let output = Command::new(cargo)
        .args(["doc", "--no-deps", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target_dir)
        .env("RUSTDOCFLAGS", rustdocflags)
        .output()
        .expect("failed to run `cargo doc`");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "`cargo doc` of the fixture failed:\n{}",
        stderr
    );

    let doc = target_dir.join("doc").join("doc_macros");
    let files = files_under(&doc);
    let has = |name: &str| files.iter().any(|file| file == Path::new(name));

    // Each kind has a page, at the crate root (the package's re-export) and in the
    // subcrate's module, and the page carries the macro's own doc comment.
    for (name, marker, note) in [
        ("my_func", "DOC-MARKER-FUNC", None),
        ("my_attr", "DOC-MARKER-ATTR", Some("apply it as")),
        ("MyDerive", "DOC-MARKER-DERIVE", Some("use it as")),
    ] {
        for page in [
            format!("macro.{}.html", name),
            format!("macros/macro.{}.html", name),
        ] {
            assert!(has(&page), "no page `{}` in {:?}", page, files);
            let html = fs::read_to_string(doc.join(&page)).unwrap();
            assert!(html.contains(marker), "`{}` lacks the doc comment", page);
            if let Some(note) = note {
                assert!(html.contains(note), "`{}` lacks the kind note", page);
            }
        }
    }

    // The `__subcrate_doc_*` stubs are hidden: no page of their own, and no mention in
    // the module listing. A `#[doc(hidden)]` macro stays hidden.
    for file in &files {
        let name = file.to_string_lossy();
        assert!(!name.contains("__subcrate"), "a page for a stub: {}", name);
        assert!(
            !name.contains("hidden_func"),
            "a page for a hidden macro: {}",
            name
        );
        if name.ends_with(".html") {
            let html = fs::read_to_string(doc.join(file)).unwrap();
            assert!(!html.contains("__subcrate_doc"), "`{}` shows a stub", name);
        }
    }
}
