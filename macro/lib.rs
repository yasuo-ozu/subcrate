//! Procedural macro backing [`subcrate`](https://docs.rs/subcrate)'s
//! `#[subcrate]` attribute.
//!
//! This crate is an implementation detail. Depend on `subcrate` and write
//! `use subcrate::subcrate;` instead.

use proc_macro::TokenStream;
use proc_macro2::Span;
use subcrate_core::{internal_crate_name, Args};
use syn::{parse_macro_input, Ident, ItemMod};
use template_quote::quote;

#[proc_macro_attribute]
pub fn subcrate(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as Args);
    let item_mod = parse_macro_input!(item as ItemMod);
    expand(args, item_mod)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand(args: Args, item_mod: ItemMod) -> syn::Result<proc_macro2::TokenStream> {
    let items = match &item_mod.content {
        Some((_, items)) => items,
        None => {
            return Err(syn::Error::new_spanned(
                &item_mod,
                "`#[subcrate]` must be applied to an inline module with a body, \
                 e.g. `#[subcrate] mod foo { .. }`",
            ))
        }
    };

    let ident = &item_mod.ident;
    // Outer attributes (doc comments, `#[cfg(..)]`, etc.) are preserved on the
    // generated binding; the attribute macro's own `#[subcrate]` invocation is
    // already stripped by the time we get here. Crate-level inner attributes
    // (`#![no_std]`, `#![allow(..)]`) instead belong on the subcrate's root and
    // are applied there by the build helper, so they are kept off the binding.
    let outer_attrs: Vec<&syn::Attribute> = item_mod
        .attrs
        .iter()
        .filter(|attr| matches!(attr.style, syn::AttrStyle::Outer))
        .collect();
    let inner = subcrate_core::inner_attrs(&item_mod.attrs);
    let vis = &item_mod.vis;

    let internal = internal_crate_name(&ident.to_string(), &args, &inner, items);

    // Confirm the build helper actually ran for this package and compiled this
    // module. If it did not, produce a clear, actionable error instead of a
    // cryptic "can't find crate" from the linker.
    let doc_deps = match verify_built(&internal) {
        Ok(Verdict::Built(deps)) => deps,
        Ok(Verdict::Skipped(reason)) => {
            // The build helper skipped this subcrate because its `#[rustversion]`
            // gate did not build on the current toolchain. If this macro is
            // expanding at all, the gate kept the module — either `#[rustversion::…]`
            // is below `#[subcrate]` (so it is still attached here, and will delete
            // this expansion on a non-matching toolchain), or the toolchain actually
            // matches and the body has a real error. Reporting the captured compiler
            // error is right in both cases: it is shown when the module is meant to
            // build here, and removed with the module when it is not.
            let message = format!(
                "the `#[subcrate]` module `{}` carries a `#[rustversion]` gate and \
                 did not build on the current toolchain.\n\n\
                 If it is meant to build here, fix the error below. If it is only for \
                 other toolchains, place `#[rustversion::\u{2026}]` *above* `#[subcrate]` \
                 so the module is removed here instead of reported.\n\n{}",
                ident, reason
            );
            return Ok(quote! {
                #(#outer_attrs)*
                #vis mod #ident {
                    ::core::compile_error!(#message);
                }
            });
        }
        Err(message) => {
            return Ok(quote! {
                #(#outer_attrs)*
                #vis mod #ident {
                    ::core::compile_error!(#message);
                }
            });
        }
    };

    // A UI error-test (`#[subcrate(should_error = "…")]`) is expected to fail to
    // compile, so the build helper produced no artifact to bind. If the build got
    // far enough to run this macro, the expectation held (otherwise the build
    // helper would have failed the build). Expand to an empty module that keeps the
    // name, visibility and outer attributes in place.
    if args.expects_error() {
        return Ok(quote! {
            #(#outer_attrs)*
            #vis mod #ident {}
        });
    }

    let internal_ident = Ident::new(&internal, Span::call_site());

    // Build (`cfg(not(doc))`): bind the name to the real, separately compiled
    // crate. This is the real expansion.
    //
    // Doc (`cfg(doc)`): rustdoc is not given the build helper's library search
    // path, so it cannot resolve the `extern crate` while documenting this
    // package. Instead we emit a module with the same name and visibility that
    // reproduces enough of the subcrate's public surface for `cargo doc` to work.
    // (A downstream package documenting its own code still goes through the real
    // crate, since dependencies are not compiled with `cfg(doc)`.)
    //
    // A library subcrate's body may name its dependencies (the real build links
    // them with `--extern`), so the stand-in imports the sibling ones from their
    // own stand-ins and the external ones from the owner's, under the same names.
    let doc_surface = if args.proc_macro {
        proc_macro_doc_surface(items, &internal)
    } else {
        let imports = doc_dependency_imports(&doc_deps, &args.deps);
        quote! { #imports #(#items)* }
    };

    Ok(quote! {
        #[cfg(not(doc))]
        #(#outer_attrs)*
        #vis extern crate #internal_ident as #ident;

        #[cfg(doc)]
        #(#outer_attrs)*
        #vis mod #ident {
            #doc_surface
        }
    })
}

/// Produces the `use` items that bring a library subcrate's dependencies into
/// scope in its `cfg(doc)` stand-in, mirroring the `--extern` names of the real
/// crate.
///
/// Each sibling dependency's stand-in lives at its module path from the crate
/// root, so `crate::<path>` reaches it. Like the real crate-root import, the names
/// are in scope at the top of the stand-in only. Only dependencies under the same
/// crate root are listed in the manifest, so a dependency declared in another root
/// has no import here. An external dependency (`::name`) is a crate the owning
/// package itself links, so `::name` reaches it directly.
fn doc_dependency_imports(
    deps: &[subcrate_core::ResolvedDep],
    declared: &[subcrate_core::Dependency],
) -> proc_macro2::TokenStream {
    let mut out = proc_macro2::TokenStream::new();
    for (alias, path) in deps {
        let alias = syn::parse_str::<Ident>(alias);
        let path = syn::parse_str::<syn::Path>(&format!("crate::{}", path.join("::")));
        if let (Ok(alias), Ok(path)) = (alias, path) {
            out.extend(quote! {
                #[allow(unused_imports)]
                use #path as #alias;
            });
        }
    }
    for dep in declared.iter().filter(|dep| dep.external) {
        let alias = syn::parse_str::<Ident>(&dep.alias);
        let path = syn::parse_str::<syn::Path>(&format!("::{}", dep.path.join("::")));
        if let (Ok(alias), Ok(path)) = (alias, path) {
            out.extend(quote! {
                #[allow(unused_imports)]
                use #path as #alias;
            });
        }
    }
    out
}

/// Produces a documentation-only stand-in for a `proc-macro` subcrate.
///
/// A `proc-macro` crate's body cannot be inlined into an ordinary crate: the
/// `#[proc_macro]` attribute is rejected outside a proc-macro crate. So for each
/// `#[proc_macro]` function we emit a `macro_rules!` stub that expands to
/// `unimplemented!()`. That lets `name::mac!(..)` call sites in any code rustdoc
/// reads resolve, whatever type is expected, without real codegen.
///
/// The stub uses `#[macro_export]` under a mangled, crate-unique name, then
/// aliases it to its real name inside the module. On stable Rust, `#[macro_export]`
/// is the only way to make a `macro_rules!` macro re-exportable by path — which a
/// user needs to lift the macro to their crate root (`pub use foo::the_macro;`).
/// The mangled name keeps that re-export from clashing with the user's own.
fn proc_macro_doc_surface(items: &[syn::Item], internal: &str) -> proc_macro2::TokenStream {
    let mut out = proc_macro2::TokenStream::new();
    for item in items {
        let func = match item {
            syn::Item::Fn(func) => func,
            _ => continue,
        };
        let is_proc_macro = func
            .attrs
            .iter()
            .any(|attr| attr.path().is_ident("proc_macro"));
        if !is_proc_macro {
            continue;
        }
        let name = &func.sig.ident;
        let exported = Ident::new(
            &format!("__subcrate_doc_{}_{}", internal, name),
            name.span(),
        );
        out.extend(quote! {
            #[macro_export]
            #[allow(unused_macros)]
            macro_rules! #exported {
                ($($__subcrate_tt:tt)*) => { ::core::unimplemented!() };
            }
            pub use crate::#exported as #name;
        });
    }
    out
}

/// Checks the metadata written by `subcrate::build::compile()` to confirm that
/// this subcrate was compiled, and returns the dependencies recorded for it.
///
/// Returns `Err(message)` describing the problem when the build helper was not
/// invoked or did not pick up this module.
/// What the build helper did with a `#[subcrate]` module, read back from its
/// manifest (see [`verify_built`]).
enum Verdict {
    /// Compiled; these are its resolved dependencies.
    Built(Vec<subcrate_core::ResolvedDep>),
    /// Skipped because its `#[rustversion]` gate did not build on this toolchain.
    /// The string is the compiler's error, captured by the build helper.
    Skipped(String),
}

fn verify_built(internal: &str) -> Result<Verdict, String> {
    let dir = match manifest_dir() {
        Some(dir) => dir,
        None => return Err(MISSING_BUILD.to_string()),
    };

    let manifest = std::path::Path::new(&dir).join(subcrate_core_manifest_name());
    let contents = match std::fs::read_to_string(&manifest) {
        Ok(contents) => contents,
        Err(_) => return Err(MISSING_BUILD.to_string()),
    };

    match subcrate_core::manifest_lookup(&contents, internal) {
        Some(subcrate_core::ManifestEntry::Built(deps)) => Ok(Verdict::Built(deps)),
        Some(subcrate_core::ManifestEntry::Skipped) => {
            // The compiler error the helper captured when the gated subcrate failed
            // to build; empty if the file is somehow missing.
            let reason = std::fs::read_to_string(
                std::path::Path::new(&dir).join(format!("{}.skipreason", internal)),
            )
            .unwrap_or_default();
            Ok(Verdict::Skipped(reason))
        }
        None => Err(format!(
            "this `#[subcrate]` module was not discovered by `subcrate::build::compile()`.\n\
             The build helper scans `src/lib.rs`, `src/main.rs`, `src/bin/*.rs`, `tests/*.rs`, \
             `benches/*.rs`, `examples/*.rs` and the modules they reach. If this module lives \
             somewhere else, or the build script is stale, run `cargo clean` and rebuild. \
             (internal name: `{}`)",
            internal
        )),
    }
}

/// Error shown when there is no sign that the build helper ran.
const MISSING_BUILD: &str = "\
`#[subcrate]` requires `subcrate::build::compile()` to run from this package's build script.\n\
Add a `build.rs` containing:\n\
\n\
    fn main() {\n\
        subcrate::build::compile().unwrap();\n\
    }\n\
\n\
and declare `subcrate` under `[build-dependencies]` in Cargo.toml.";

/// The directory holding the build helper's metadata.
///
/// `SUBCRATE_MANIFEST_DIR` is set by `subcrate::build::compile()` via
/// `cargo:rustc-env`, so its mere presence proves the build helper ran during
/// this compilation. We deliberately do **not** fall back to `OUT_DIR`: a
/// build script that exists but never calls `compile()` still has an `OUT_DIR`,
/// and a stale metadata file there could mask the "helper not invoked" error.
fn manifest_dir() -> Option<String> {
    std::env::var("SUBCRATE_MANIFEST_DIR").ok()
}

fn subcrate_core_manifest_name() -> &'static str {
    "subcrate-manifest.txt"
}
