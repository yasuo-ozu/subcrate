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

Illustrative run — 24 cases, 20-core Linux (heavily oversubscribed: 24 parallel
case compiles), `rustc 1.101.0-nightly`, with `safegraph 0.3.2` (`macros` feature
off) and `template-quote 0.5`. The warm rows are the best (min) of several runs;
cold is the median of several true clean builds, because the in-script cold row is
a single, wildly swinging sample. Absolute numbers depend on the machine and
toolchain — the *ratios* are the point.

| scenario (seconds)              | subcrate    | trybuild    |
|---------------------------------|-------------|-------------|
| cold (incl. framework build)    | ~10.6       | ~8.2        |
| no-op re-run (min)              | **0.18**    | 0.52        |
| one case edited (min)           | 0.75        | 0.74        |
| all 24 cases rebuilt (min)      | **0.47**    | 0.85        |

Reading it:

- **Cold is now roughly level.** The crate here depends on `subcrate` with
  `default-features = false` (only the attribute is needed at compile time), so the
  build helper compiles once. It used to compile **two** full `syn` majors — `v2`
  for the helper and `v1` dragged in by `safegraph`'s macro crate
  (`safegraph-macros` → `proc-macro-error`). `safegraph 0.3.2` puts that macro crate
  behind a non-default `macros` feature, and subcrate — which uses only safegraph's
  toposort, not its `graph!` macro — doesn't enable it, so `syn v1` is gone and only
  `syn v2` remains. That roughly halves the cold build and brings it level with
  trybuild's `serde` / `toml` / `syn` tree. A one-time, amortized cost either way.
- **The no-op re-run is subcrate's clear win.** With dependencies built, subcrate
  has no per-run nested-`cargo` startup and skips unchanged cases by content hash,
  so re-running with nothing changed (the "did my other edit break a test?" loop)
  is several times faster. trybuild spawns a nested `cargo` on every run.
- **Recompiles are comparable.** A single-case edit is about even and a full
  rebuild edges to subcrate: both hand *N* (or one) crates to `rustc` and
  parallelize, but subcrate skips trybuild's nested-`cargo` layer, which tells more
  on a bulk rebuild. The sub-second gaps are noisy — read them as "subcrate no
  worse," not a decisive win.

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
