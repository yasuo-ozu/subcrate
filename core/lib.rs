//! Shared logic between the `subcrate` build helper and the `subcrate-macros`
//! procedural macro.
//!
//! The two sides never talk to each other directly. Instead they must,
//! independently, agree on a single thing: the **internal crate name** that a
//! given `#[subcrate]` module compiles to. The build helper uses it to name the
//! `rustc` output; the procedural macro uses it to emit the `extern crate`
//! binding that loads that output. Because both derive the name purely from the
//! module's identifier and body, they always arrive at the same answer without
//! any coordination.

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{AttrStyle, Attribute, Expr, ExprLit, Item, Lit, Meta, MetaNameValue, Token};
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
/// `#[subcrate(proc_macro = true, edition = "2018")]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Args {
    /// Compile the module as a `proc-macro` crate rather than an ordinary
    /// library crate.
    pub proc_macro: bool,
    /// Compile the subcrate with this edition instead of the owning package's.
    pub edition: Option<String>,
}

impl Args {
    /// A stable textual form of the arguments, folded into the internal crate
    /// name so that two otherwise-identical module bodies compiled with
    /// different arguments (e.g. different editions) get distinct crates.
    fn fingerprint(&self) -> String {
        format!(
            "{}|{}",
            self.proc_macro,
            self.edition.as_deref().unwrap_or("")
        )
    }
}

impl Parse for Args {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut args = Args::default();
        let metas = Punctuated::<MetaNameValue, Token![,]>::parse_terminated(input)?;
        for meta in metas {
            if meta.path.is_ident("proc_macro") {
                args.proc_macro = parse_bool(&meta.value)?;
            } else if meta.path.is_ident("edition") {
                args.edition = Some(parse_string(&meta.value)?);
            } else {
                return Err(syn::Error::new_spanned(
                    &meta.path,
                    "unknown `#[subcrate]` argument (expected `proc_macro` or `edition`)",
                ));
            }
        }
        Ok(args)
    }
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

/// Produces the canonical textual form of a module body.
///
/// Both the macro and the build helper obtain the same `Vec<Item>` (one from the
/// token stream handed to the attribute macro, the other from re-parsing the
/// source file) and render it through the same [`template_quote::quote!`]
/// pipeline.
///
/// Crucially, we do **not** hash `TokenStream::to_string()`: that rendering
/// differs between the real proc-macro backend (used inside the macro) and
/// `proc_macro2`'s fallback backend (used inside the build script), mostly in
/// whitespace. Instead we walk the token tree and emit a representation that
/// depends only on token *structure and text*, never on spacing, so the two
/// backends agree byte-for-byte.
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
            // Literals are wrapped in markers so their boundaries are explicit.
            TokenTree::Literal(lit) => {
                out.push('\u{2045}');
                out.push_str(&lit.to_string());
                out.push('\u{2046}');
            }
        }
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
