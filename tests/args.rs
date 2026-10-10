//! Tests for parsing `#[subcrate(...)]` arguments and for the content-addressed
//! internal crate name they feed into.

use subcrate_core::{internal_crate_name, Args, Dependency};

fn parse(src: &str) -> syn::Result<Args> {
    syn::parse_str::<Args>(src)
}

fn dep(alias: &str, path: &[&str]) -> Dependency {
    Dependency {
        alias: alias.to_string(),
        path: path.iter().map(|s| s.to_string()).collect(),
        external: false,
    }
}

fn name(args: &Args) -> String {
    internal_crate_name("m", args, &[], &[])
}

#[test]
fn empty_and_flags() {
    assert_eq!(parse("").unwrap(), Args::default());
    assert!(parse("proc_macro").unwrap().proc_macro);
    assert!(parse("proc_macro = true").unwrap().proc_macro);
    assert!(!parse("proc_macro = false").unwrap().proc_macro);
    assert_eq!(
        parse("edition = \"2018\"").unwrap().edition.as_deref(),
        Some("2018")
    );
}

#[test]
fn bare_ident_is_a_dependency_under_its_own_name() {
    let args = parse("a").unwrap();
    assert_eq!(args.deps, vec![dep("a", &["a"])]);
}

#[test]
fn path_dependency_is_named_by_its_last_segment() {
    let args = parse("outer::inner::a").unwrap();
    assert_eq!(args.deps, vec![dep("a", &["outer", "inner", "a"])]);
    // A leading `crate` is just the crate root.
    assert_eq!(
        parse("crate::outer::a").unwrap(),
        parse("outer::a").unwrap()
    );
}

#[test]
fn alias_renames_a_dependency() {
    let args = parse("x = outer::a, y = b").unwrap();
    assert_eq!(args.deps, vec![dep("x", &["outer", "a"]), dep("y", &["b"])]);
}

#[test]
fn value_type_disambiguates_edition_from_an_alias() {
    // A string literal is the `edition` key; a path is a dependency that
    // happens to be named `edition`.
    let args = parse("edition = a").unwrap();
    assert_eq!(args.edition, None);
    assert_eq!(args.deps, vec![dep("edition", &["a"])]);

    let args = parse("edition = \"2021\", edition = a").unwrap();
    assert_eq!(args.edition.as_deref(), Some("2021"));
    assert_eq!(args.deps, vec![dep("edition", &["a"])]);
}

#[test]
fn everything_together() {
    let args = parse("proc_macro, edition = \"2018\", a, x = nested::b").unwrap();
    assert!(args.proc_macro);
    assert_eq!(args.edition.as_deref(), Some("2018"));
    assert_eq!(
        args.deps,
        vec![dep("a", &["a"]), dep("x", &["nested", "b"])]
    );
}

#[test]
fn external_crates_are_parsed_but_flagged() {
    let args = parse("::serde, j = ::serde_json::value").unwrap();
    assert_eq!(args.deps.len(), 2);
    assert!(args.deps.iter().all(|d| d.external));
    assert_eq!(args.deps[0].alias, "serde");
    assert_eq!(args.deps[1].alias, "j");
    assert_eq!(args.deps[1].path_display(), "::serde_json::value");
}

#[test]
fn proc_macro_is_reserved() {
    assert!(parse("proc_macro = a").is_err());
    assert!(parse("proc_macro = \"yes\"").is_err());
}

#[test]
fn malformed_arguments_are_rejected() {
    assert!(parse("edition = true").is_err());
    assert!(parse("unknown = \"x\"").is_err());
    assert!(parse("uses(a)").is_err());
    assert!(parse("super::a").is_err());
    assert!(parse("self::a").is_err());
    assert!(parse("a::<T>").is_err());
    assert!(parse("crate").is_err());
}

#[test]
fn dependencies_are_part_of_the_internal_name() {
    let none = name(&parse("").unwrap());
    let with_a = name(&parse("a").unwrap());
    let with_aliased_a = name(&parse("x = a").unwrap());
    let with_other_path = name(&parse("nested::a").unwrap());
    assert_ne!(none, with_a);
    assert_ne!(with_a, with_aliased_a);
    assert_ne!(with_a, with_other_path);
}

#[test]
fn dependency_order_does_not_change_the_internal_name() {
    assert_eq!(name(&parse("a, b").unwrap()), name(&parse("b, a").unwrap()));
    // `crate::` is just the crate root, so it does not change the name either.
    assert_eq!(
        name(&parse("crate::a").unwrap()),
        name(&parse("a").unwrap())
    );
}
