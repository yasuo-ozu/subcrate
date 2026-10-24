//! Shared logic between the `subcrate` build helper and the `subcrate-macros`
//! procedural macro.
//!
//! The two sides never talk to each other. They only have to agree, on their own,
//! about one thing: the **internal crate name** a given `#[subcrate]` module
//! compiles to. The build helper uses it to name the `rustc` output; the
//! procedural macro uses it in the `extern crate` binding that loads that output.
//! Both derive the name from the module's identifier and body alone, so they
//! always reach the same answer with no coordination.

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    AttrStyle, Attribute, Expr, ExprLit, ExprPath, Item, Lit, Meta, MetaNameValue, PathArguments,
    Token,
};
use template_quote::quote;

/// The prefix of every generated internal crate name.
///
/// It is deliberately unlikely to collide with a real crate name and makes the
/// generated artifacts easy to recognise in a `target` directory. It uses a
/// single trailing underscore (not `__`) so the full name stays snake-case and
/// never trips the `non_snake_case` lint — which would otherwise become an error
/// whenever the owner builds with `-D warnings`.
pub const INTERNAL_PREFIX: &str = "__subcrate_";

/// Arguments accepted by the `#[subcrate]` attribute, e.g.
/// `#[subcrate(proc_macro, edition = "2018", other_subcrate)]`.
///
/// The attribute takes a flat, comma-separated list. Each argument is one of:
///
/// * `proc_macro` (or `proc_macro = true`) — compile as a `proc-macro` crate.
/// * `edition = "2018"` — compile with this edition.
/// * `path::to::dep` — depend on the sibling subcrate at that module path,
///   visible inside this subcrate under the path's last segment.
/// * `alias = path::to::dep` — the same, visible under `alias`.
/// * `::name` (or `alias = ::name`) — an *external* crate: one the owning
///   package depends on, or — if `name` is the package's own library — the owner
///   crate itself. The build helper has Cargo build it for the subcrate. It is
///   visible inside the subcrate under `name` (or `alias`).
///
/// The remaining arguments turn the subcrate into a **UI test** — a build-time
/// assertion about the diagnostics `rustc` produces for it. Each may be given more
/// than once, and every one must hold (they are combined with AND — e.g. two
/// `should_error = "…"` require a matching error for *each* string):
///
/// * `should_error` — the subcrate must *fail* to compile. Any `should_error`
///   makes it expected to fail, so it produces no artifact and its module expands
///   to an empty stand-in. With `= "…"` an error's rendered text must also contain
///   that string.
/// * `should_warn` — the subcrate must emit at least one warning. With `= "…"` a
///   warning's rendered text must contain that string.
/// * `should_spanned = "…"` — some diagnostic must underline source code equal to
///   this string once whitespace is removed from both sides. (A value is required.)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Args {
    /// Compile the module as a `proc-macro` crate rather than an ordinary
    /// library crate.
    pub proc_macro: bool,
    /// Compile the subcrate with this edition instead of the owning package's.
    pub edition: Option<String>,
    /// The crates this subcrate links against, in the order written.
    pub deps: Vec<Dependency>,
    /// The UI-test expectations (`should_error`/`should_warn`/`should_spanned`).
    pub ui: UiExpectations,
}

impl Args {
    /// Whether these arguments make the subcrate a UI test (any `should_*`).
    pub fn is_ui_test(&self) -> bool {
        self.ui.is_ui_test()
    }

    /// Whether the subcrate is expected to fail to compile (any `should_error`),
    /// so it produces no artifact.
    pub fn expects_error(&self) -> bool {
        self.ui.expects_error()
    }
}

/// A subcrate's UI-test expectations: assertions about the diagnostics `rustc`
/// produces for it, from `#[subcrate(should_error/should_warn/should_spanned …)]`.
///
/// Any non-empty field makes the subcrate a UI test. Every expectation must hold
/// — they are combined with AND.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UiExpectations {
    /// One entry per `should_error`: `Some(s)` requires an error whose rendered
    /// text contains `s`, `None` (bare `should_error`) only that compilation
    /// fails. A non-empty list makes the subcrate a UI test expected to fail.
    pub should_errors: Vec<Option<String>>,
    /// One entry per `should_warn`: `Some(s)` requires a warning whose rendered
    /// text contains `s`, `None` (bare `should_warn`) only that some warning is
    /// emitted.
    pub should_warns: Vec<Option<String>>,
    /// Code a diagnostic must underline, compared with whitespace removed.
    pub should_spanned: Vec<String>,
}

impl UiExpectations {
    /// Whether any expectation is set, so the subcrate is a UI test.
    pub fn is_ui_test(&self) -> bool {
        !self.should_errors.is_empty()
            || !self.should_warns.is_empty()
            || !self.should_spanned.is_empty()
    }

    /// Whether the subcrate is expected to fail to compile (any `should_error`),
    /// so it produces no artifact.
    pub fn expects_error(&self) -> bool {
        !self.should_errors.is_empty()
    }

    /// A stable textual form folded into the internal crate name (see
    /// [`Args::fingerprint`]): `S<text>` for a `Some`, `N` for a bare (`None`)
    /// occurrence.
    fn fingerprint(&self) -> String {
        format!(
            "e:{}|w:{}|s:{}",
            fingerprint_opts(&self.should_errors),
            fingerprint_opts(&self.should_warns),
            self.should_spanned.join("\u{1f}"),
        )
    }
}

/// One dependency of a subcrate, as written in `#[subcrate(...)]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// The name the dependency is visible under inside the subcrate: the explicit
    /// `alias = …`, or else the last segment of `path`. (The build helper imports
    /// the dependency's crate under this name at the subcrate's crate root.)
    pub alias: String,
    /// The module path of the depended-on subcrate, from its crate root, with
    /// any leading `crate` removed (e.g. `["nested", "inner"]`). For an
    /// external crate this is the crate's path after the leading `::`.
    pub path: Vec<String>,
    /// `true` for a leading-`::` path naming an external crate: one the owning
    /// package depends on, or the owner crate itself. (The build helper accepts
    /// just the crate's name, not a path into it.)
    pub external: bool,
}

impl Dependency {
    /// The dependency's path as written, e.g. `nested::inner` or `::serde`.
    pub fn path_display(&self) -> String {
        format!(
            "{}{}",
            if self.external { "::" } else { "" },
            self.path.join("::")
        )
    }
}

impl Args {
    /// A stable textual form of the arguments, folded into the internal crate
    /// name so that two otherwise-identical module bodies compiled with
    /// different arguments (e.g. different editions or dependencies) get
    /// distinct crates.
    ///
    /// Dependencies are sorted, so reordering them does not change the name. Only
    /// their *paths* are folded in, not the depended-on crate's content: the
    /// build helper recompiles a subcrate whenever a dependency's crate name
    /// (which embeds that dependency's content hash) changes.
    ///
    /// The UI-test expectations are folded in too, so editing a `should_error`
    /// (or sibling) gives a fresh crate name and re-runs the check. They do not
    /// change what is compiled, only what the build helper asserts about the
    /// result, but a UI test has no downstream users to churn.
    fn fingerprint(&self) -> String {
        let mut deps: Vec<String> = self
            .deps
            .iter()
            .map(|dep| format!("{}={}", dep.alias, dep.path_display()))
            .collect();
        deps.sort();
        format!(
            "{}|{}|{}|{}",
            self.proc_macro,
            self.edition.as_deref().unwrap_or(""),
            deps.join(","),
            self.ui.fingerprint(),
        )
    }
}

/// A stable textual form of a `should_error`/`should_warn` list for the
/// fingerprint: `S<text>` for `Some`, `N` for a bare (`None`) occurrence.
fn fingerprint_opts(opts: &[Option<String>]) -> String {
    opts.iter()
        .map(|opt| match opt {
            Some(text) => format!("S{}", text),
            None => "N".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

impl Parse for Args {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut args = Args::default();
        let metas = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        for meta in metas {
            match &meta {
                // A bare path: the `proc_macro` flag, a bare UI-test assertion, or
                // a dependency.
                Meta::Path(path) => {
                    if path.is_ident("proc_macro") {
                        args.proc_macro = true;
                    } else if path.is_ident("should_error") {
                        args.ui.should_errors.push(None);
                    } else if path.is_ident("should_warn") {
                        args.ui.should_warns.push(None);
                    } else if path.is_ident("should_spanned") {
                        return Err(syn::Error::new_spanned(
                            path,
                            "`should_spanned` needs the code to match: \
                             `should_spanned = \"…\"`",
                        ));
                    } else {
                        args.deps.push(parse_dependency(None, path)?);
                    }
                }
                Meta::NameValue(MetaNameValue { path, value, .. }) => {
                    let key = match path.get_ident() {
                        Some(ident) => ident.to_string(),
                        None => return Err(unknown_argument(path)),
                    };
                    // The value's kind picks the meaning: a path renames a
                    // dependency, a literal sets a known key.
                    match value {
                        Expr::Path(ExprPath {
                            qself: None,
                            path: target,
                            ..
                        }) => {
                            if key == "proc_macro" {
                                return Err(syn::Error::new_spanned(
                                    path,
                                    "`proc_macro` is reserved and cannot name a dependency",
                                ));
                            }
                            args.deps.push(parse_dependency(Some(key), target)?);
                        }
                        _ if key == "proc_macro" => args.proc_macro = parse_bool(value)?,
                        _ if key == "edition" => args.edition = Some(parse_string(value)?),
                        _ if key == "should_error" => {
                            args.ui.should_errors.push(Some(parse_string(value)?))
                        }
                        _ if key == "should_warn" => {
                            args.ui.should_warns.push(Some(parse_string(value)?))
                        }
                        _ if key == "should_spanned" => {
                            args.ui.should_spanned.push(parse_string(value)?)
                        }
                        _ => return Err(unknown_argument(path)),
                    }
                }
                Meta::List(list) => return Err(unknown_argument(&list.path)),
            }
        }
        Ok(args)
    }
}

fn unknown_argument(path: &syn::Path) -> syn::Error {
    syn::Error::new_spanned(
        path,
        "unknown `#[subcrate]` argument (expected `proc_macro`, `edition = \"…\"`, a \
         dependency: `path::to::subcrate`, `::crate_name`, or either with an `alias = `, or a \
         UI-test assertion: `should_error`, `should_warn` (each optionally `= \"…\"`), or \
         `should_spanned = \"…\"`)",
    )
}

/// Parses one dependency path, optionally renamed to `alias`.
///
/// A single leading `crate` is dropped (module paths are relative to the crate
/// root); `self`/`super`/`Self` and generic arguments are rejected, since a
/// subcrate has no meaningful position to resolve them against.
fn parse_dependency(alias: Option<String>, path: &syn::Path) -> syn::Result<Dependency> {
    let external = path.leading_colon.is_some();
    let mut segments = Vec::new();
    for segment in &path.segments {
        if !matches!(segment.arguments, PathArguments::None) {
            return Err(syn::Error::new_spanned(
                segment,
                "a subcrate dependency path cannot have generic arguments",
            ));
        }
        segments.push(segment.ident.to_string());
    }
    if !external && segments.first().map(String::as_str) == Some("crate") {
        segments.remove(0);
    }
    if segments
        .iter()
        .any(|s| matches!(s.as_str(), "crate" | "self" | "super" | "Self"))
    {
        return Err(syn::Error::new_spanned(
            path,
            "a subcrate dependency path must name a module from the crate root, \
             like `outer::inner` (`self`, `super` and `Self` are not allowed)",
        ));
    }
    let last = match segments.last() {
        Some(last) => last.clone(),
        None => {
            return Err(syn::Error::new_spanned(
                path,
                "expected the path of a subcrate",
            ))
        }
    };
    Ok(Dependency {
        alias: alias.unwrap_or(last),
        path: segments,
        external,
    })
}

fn parse_bool(expr: &Expr) -> syn::Result<bool> {
    if let Expr::Lit(ExprLit {
        lit: Lit::Bool(b), ..
    }) = expr
    {
        Ok(b.value)
    } else {
        Err(syn::Error::new_spanned(expr, "expected a boolean literal"))
    }
}

fn parse_string(expr: &Expr) -> syn::Result<String> {
    if let Expr::Lit(ExprLit {
        lit: Lit::Str(s), ..
    }) = expr
    {
        Ok(s.value())
    } else {
        Err(syn::Error::new_spanned(expr, "expected a string literal"))
    }
}

/// Returns the crate-level inner attributes (`#![…]`) of a module body — the
/// ones that belong on the subcrate's crate root, such as `#![no_std]` or
/// `#![allow(…)]`.
pub fn inner_attrs(attrs: &[Attribute]) -> Vec<Attribute> {
    attrs
        .iter()
        .filter(|attr| matches!(attr.style, AttrStyle::Inner(_)))
        .cloned()
        .collect()
}

/// Returns `true` if `attr` is the `#[subcrate]` attribute.
///
/// Matches both the imported form (`#[subcrate]`, via `use subcrate::subcrate;`)
/// and the fully qualified form (`#[subcrate::subcrate]`) by comparing only the
/// final path segment.
pub fn is_subcrate_attr(attr: &Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .map(|seg| seg.ident == "subcrate")
        .unwrap_or(false)
}

/// Parses the [`Args`] carried by a `#[subcrate(...)]` attribute as it appears
/// in parsed source (used by the build helper).
pub fn parse_attr_args(attr: &Attribute) -> syn::Result<Args> {
    match &attr.meta {
        Meta::Path(_) => Ok(Args::default()),
        Meta::List(list) => syn::parse2::<Args>(list.tokens.clone()),
        Meta::NameValue(nv) => Err(syn::Error::new_spanned(
            nv,
            "`#[subcrate]` does not take a `= value` form",
        )),
    }
}

/// A sibling dependency of a subcrate that the build helper resolved *under the
/// same crate root*, as recorded in the manifest: the name it is visible under
/// (the alias) and its module path from that root.
pub type ResolvedDep = (String, Vec<String>);

/// The reserved first field marking a manifest line as a skipped subcrate (one
/// whose `#[rustversion]` gate kept it from building on the current toolchain).
/// A real dependency field is always `alias=path`, and an alias is an identifier,
/// so this `!`-prefixed token can never collide with one.
pub const SKIPPED_MARKER: &str = "!skipped";

/// What the build helper recorded for a subcrate in its manifest.
pub enum ManifestEntry {
    /// The subcrate was compiled; these are its resolved dependencies (used to
    /// give its `cfg(doc)` stand-in the same names the real crate gets).
    Built(Vec<ResolvedDep>),
    /// The subcrate carries a `#[rustversion]` gate and did not build on the
    /// current toolchain, so no artifact exists. The macro expands it away. See
    /// `subcrate::build::compile`'s handling of rustversion-gated subcrates.
    Skipped,
}

/// Renders one line of the build helper's manifest: the subcrate's internal name,
/// then one tab-separated `alias=path::to::dep` per resolved dependency.
///
/// The macro reads the dependencies back to give a subcrate's `cfg(doc)`
/// stand-in the same names in scope that the real crate gets from `--extern`.
pub fn manifest_line(internal: &str, deps: &[ResolvedDep]) -> String {
    let mut line = internal.to_string();
    for (alias, path) in deps {
        line.push('\t');
        line.push_str(alias);
        line.push('=');
        line.push_str(&path.join("::"));
    }
    line
}

/// Renders the manifest line for a subcrate the helper skipped because its
/// `#[rustversion]` gate did not build on the current toolchain (see
/// [`ManifestEntry::Skipped`]).
pub fn skipped_manifest_line(internal: &str) -> String {
    format!("{}\t{}", internal, SKIPPED_MARKER)
}

/// Finds the manifest line for `internal` (see [`manifest_line`]) and returns what
/// the helper recorded for it, or `None` if the subcrate is not listed.
pub fn manifest_lookup(manifest: &str, internal: &str) -> Option<ManifestEntry> {
    manifest.lines().find_map(|line| {
        let mut fields = line.trim().split('\t');
        if fields.next()? != internal {
            return None;
        }
        let rest = fields.collect::<Vec<&str>>();
        if rest.first() == Some(&SKIPPED_MARKER) {
            return Some(ManifestEntry::Skipped);
        }
        Some(ManifestEntry::Built(
            rest.into_iter()
                .filter_map(|field| {
                    let (alias, path) = field.split_once('=')?;
                    let path = path.split("::").map(str::to_string).collect();
                    Some((alias.to_string(), path))
                })
                .collect(),
        ))
    })
}

/// Produces a canonical text form of a module body.
///
/// The macro and the build helper each start from the same `Vec<Item>` — one from
/// the tokens handed to the attribute macro, the other from re-parsing the source
/// file — and render it the same way.
///
/// We do **not** hash `TokenStream::to_string()`. That text differs between the
/// real proc-macro backend (used in the macro) and `proc_macro2`'s fallback
/// backend (used in the build script) — in whitespace, and in how a string
/// literal is spelled (raw vs escaped). Instead we walk the token tree and write a
/// form that depends only on each token's kind and, for literals, decoded value —
/// not its spacing or spelling — so both backends produce the same bytes.
pub fn canonical_body(attrs: &[Attribute], items: &[Item]) -> String {
    let tokens: TokenStream = quote! { #(#attrs)* #(#items)* };
    let mut out = String::new();
    write_canonical(&tokens, &mut out);
    out
}

fn write_canonical(tokens: &TokenStream, out: &mut String) {
    for tt in tokens.clone() {
        match tt {
            TokenTree::Group(group) => {
                let (open, close) = match group.delimiter() {
                    Delimiter::Parenthesis => ('(', ')'),
                    Delimiter::Brace => ('{', '}'),
                    Delimiter::Bracket => ('[', ']'),
                    // Invisible groups: use markers that cannot appear in source.
                    Delimiter::None => ('\u{2039}', '\u{203a}'),
                };
                out.push(open);
                write_canonical(&group.stream(), out);
                out.push(close);
            }
            // Idents are space-delimited so `a b` never collapses into `ab`.
            TokenTree::Ident(ident) => {
                out.push(' ');
                out.push_str(&ident.to_string());
                out.push(' ');
            }
            // Punct characters are concatenated verbatim; multi-character
            // operators arrive as separate joint puncts and reassemble exactly.
            TokenTree::Punct(punct) => out.push(punct.as_char()),
            // Literals are wrapped in markers so their boundaries are explicit,
            // and reduced to their value (see `canonical_literal`).
            TokenTree::Literal(lit) => {
                out.push('\u{2045}');
                out.push_str(&canonical_literal(&lit.to_string()));
                out.push('\u{2046}');
            }
        }
    }
}

/// Reduces a literal to a backend-independent form. A string, byte-string, char
/// or byte literal becomes its decoded value, so the raw and escaped spellings of
/// one value agree: the two proc-macro backends spell the same value differently
/// (a raw `r"a\b"` on one, an escaped `"a\\b"` on the other — notably for a doc
/// comment carrying `"` or `\`), and hashing the spelling would make the macro and
/// the build helper disagree on a module's name. Numeric literals render the same
/// on both backends, and anything that does not parse as a literal is left as is.
fn canonical_literal(text: &str) -> String {
    match syn::parse_str::<Lit>(text) {
        Ok(Lit::Str(s)) => format!("s\u{1f}{}", s.value()),
        Ok(Lit::ByteStr(s)) => format!("b\u{1f}{:?}", s.value()),
        Ok(Lit::Char(c)) => format!("c\u{1f}{}", c.value()),
        Ok(Lit::Byte(b)) => format!("y\u{1f}{}", b.value()),
        _ => text.to_string(),
    }
}

/// Computes the internal crate name for a `#[subcrate]` module.
///
/// The name is content-addressed: it embeds a hash of the module identifier and
/// its body. This has two useful consequences:
///
/// * The macro and the build helper compute the same name with no shared state.
/// * When the body (its items, crate-level inner attributes, or arguments)
///   changes, the name changes, so stale artifacts are never reused — and when
///   it does not change, the existing artifact is reused as-is (incremental
///   rebuilds).
///
/// `inner_attrs` are the subcrate's crate-root `#![…]` attributes; `args` are
/// the `#[subcrate(...)]` arguments. Both the macro and the build helper pass
/// the same values, so they agree.
pub fn internal_crate_name(
    ident: &str,
    args: &Args,
    inner_attrs: &[Attribute],
    items: &[Item],
) -> String {
    let body = canonical_body(inner_attrs, items);
    let mut hasher = Fnv1a::new();
    // Hash the original identifier for uniqueness; the name only *displays* a
    // sanitized form of it.
    hasher.write(ident.as_bytes());
    hasher.write(&[0x1f]);
    hasher.write(args.fingerprint().as_bytes());
    hasher.write(&[0x1f]);
    hasher.write(body.as_bytes());
    format!(
        "{}{}_{:016x}",
        INTERNAL_PREFIX,
        sanitize_ident(ident),
        hasher.finish()
    )
}

/// Reduces an identifier to a lowercase, single-underscore-separated token so the
/// generated crate name is valid snake case regardless of the original casing.
/// Uniqueness does not rely on this (the hash covers the exact identifier).
fn sanitize_ident(ident: &str) -> String {
    let mut result = String::new();
    let mut gap = false;
    for ch in ident.chars() {
        if ch.is_ascii_alphanumeric() {
            // A single underscore joins alphanumeric runs; leading/trailing runs
            // of non-alphanumerics are dropped, so no `__` can appear.
            if gap && !result.is_empty() {
                result.push('_');
            }
            result.push(ch.to_ascii_lowercase());
            gap = false;
        } else {
            gap = true;
        }
    }
    if result.is_empty() {
        "anon".to_string()
    } else {
        result
    }
}

/// A tiny, dependency-free FNV-1a (64-bit) hasher.
///
/// `std`'s `DefaultHasher` is deterministic today, but its output is explicitly
/// not guaranteed stable. We need a value that is identical across two separate
/// compilations (the build script and the proc-macro host), so we spell the
/// algorithm out here.
struct Fnv1a(u64);

impl Fnv1a {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Fnv1a(Self::OFFSET)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A raw and an escaped spelling of one string value must canonicalize the
    // same, so the macro and the build helper (whose proc-macro backends spell
    // literals differently) agree on the internal name. Regression test for the
    // "a doc comment with `\"` or `\\` breaks discovery" bug.
    #[test]
    fn literals_canonicalize_by_value_not_spelling() {
        let raw: Item = syn::parse_str(r##"fn f() -> &'static str { r"a\b" }"##).unwrap();
        let escaped: Item = syn::parse_str(r#"fn f() -> &'static str { "a\\b" }"#).unwrap();
        assert_eq!(
            canonical_body(&[], std::slice::from_ref(&raw)),
            canonical_body(&[], std::slice::from_ref(&escaped)),
        );
        assert_eq!(
            internal_crate_name("m", &Args::default(), &[], std::slice::from_ref(&raw)),
            internal_crate_name("m", &Args::default(), &[], std::slice::from_ref(&escaped)),
        );
    }

    // A skipped subcrate (its `#[rustversion]` gate kept it from building on this
    // toolchain) is recorded distinctly from a built one, and the marker can never
    // be confused with a dependency field (which is always `alias=path`).
    #[test]
    fn a_skipped_subcrate_round_trips_through_the_manifest() {
        let built = manifest_line(
            "__subcrate_a",
            &[("dep".into(), vec!["x".into(), "y".into()])],
        );
        let skipped = skipped_manifest_line("__subcrate_b");
        let manifest = format!("{}\n{}\n", built, skipped);

        match manifest_lookup(&manifest, "__subcrate_a") {
            Some(ManifestEntry::Built(deps)) => {
                assert_eq!(
                    deps,
                    vec![("dep".to_string(), vec!["x".to_string(), "y".to_string()])]
                );
            }
            _ => panic!("expected a built entry"),
        }
        assert!(matches!(
            manifest_lookup(&manifest, "__subcrate_b"),
            Some(ManifestEntry::Skipped)
        ));
        assert!(manifest_lookup(&manifest, "__subcrate_missing").is_none());
    }
}
