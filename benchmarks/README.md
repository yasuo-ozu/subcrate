# Benchmark: `subcrate` UI testing vs. [`trybuild`]

Both frameworks answer the same question — *does this snippet fail to compile
with the diagnostic I expect?* — but by different means:

- **subcrate** checks each case at **build time**. Every
  `#[subcrate(should_error = "…")]` module is compiled with `rustc` directly
  (in parallel, artifacts reused by content hash) from the package's build
  script, so a plain `cargo build`/`cargo test` runs the checks.
- **trybuild** checks at **test time**. A `#[test]` calls
  `trybuild::TestCases::compile_fail("tests/ui/*.rs")`, which spawns a nested
  `cargo` to compile the cases and compares each one's stderr against a checked-in
  `.stderr` snapshot.

This benchmark runs an identical set of *N* compile-fail cases (the same one-line
`E0308` type error) through each and times the common edit/run loops.

## Running it

```sh
cd benchmarks
N=24 bash run.sh      # tunables: N, NOOP_SAMPLES, INC_SAMPLES, ALL_SAMPLES
```

`run.sh` generates the cases (`gen.sh`), warms the caches, records trybuild's
`.stderr` snapshots, then times four scenarios and prints a table. It is a
separate Cargo workspace (see the root `Cargo.toml`'s `exclude`) so trybuild's
dependency tree never enters this crate's 1.56-locked `Cargo.lock`.

## Scenarios

| scenario | what it measures |
|----------|------------------|
| **cold** | `cargo clean` then build — includes compiling the framework's own dependencies |
| **no-op** | re-run with nothing changed (the "did my other edit break a test?" loop) |
| **one case edited** | change a single case, re-run (deps and other cases warm) |
| **all N rebuilt** | change every case, re-run (deps warm) |

## Results

Illustrative run — 24 cases, 20-core Linux, `rustc 1.101.0-nightly`. Absolute
numbers depend heavily on the machine and toolchain; the *ratios* are the point.

| scenario (seconds)              | subcrate    | trybuild    |
|---------------------------------|-------------|-------------|
| cold (incl. framework build)    | 21.3        | **19.5**    |
| no-op re-run (min)              | **0.29**    | 1.09        |
| one case edited (min)           | **0.82**    | 1.08        |
| all 24 cases rebuilt (min)      | **0.91**    | 1.08        |

Reading it:

- **Cold is close, trybuild just ahead.** The crate here depends on `subcrate`
  with `default-features = false` (only the attribute is needed at compile time),
  so the build helper compiles once. What remains is two full `syn` builds — `v2`
  for the helper and `v1` dragged in by `template-quote`'s older `proc-quote` /
  `proc-macro-error` chain — plus `safegraph` for the dependency toposort, against
  trybuild's single derive-only `syn` and serde. A one-time, amortized cost; drop
  the `syn v1` chain from `template-quote` and the two would be level.
- **The warm dev loop is subcrate's.** With dependencies built, subcrate has no
  per-run nested-`cargo` startup and skips unchanged cases by content hash, so a
  no-op re-run is ~3–4× faster and a single-case edit noticeably faster. trybuild
  pays a roughly fixed nested-`cargo` cost (~1 s here) on every run.
- **Bulk recompiles slightly favor subcrate** — both ask `rustc` to compile *N*
  crates and both parallelize, but subcrate skips the nested-`cargo` layer.

## Fairness notes

- subcrate's cases share one source file, so editing one case also recompiles the
  (tiny) test crate; trybuild's cases are separate files. This slightly favors
  trybuild in "one case edited".
- A subcrate `should_error` case produces **no artifact** (it only has to fail),
  while trybuild compiles each case as a full crate — a real model difference, not
  a thumb on the scale.
- In "one case edited" and "all rebuilt" the trybuild `.stderr` snapshots no
  longer match the mutated source, so its test *reports* a mismatch; the compile
  work — the part being timed — is identical either way.
- The two tools are not interchangeable: trybuild needs no build script and its
  cases are standalone files, while subcrate's cases are inline modules checked by
  the same build that compiles the crate. Pick on ergonomics first; these numbers
  are about the loop's cost, not which to use.

[`trybuild`]: https://crates.io/crates/trybuild
