#!/usr/bin/env bash
# Benchmark subcrate UI testing against trybuild on an identical set of
# compile-fail cases. See README.md for what each scenario means and the
# fairness caveats. Tunables (env): N, NOOP_SAMPLES, INC_SAMPLES, ALL_SAMPLES.
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
N="${N:-24}"
NOOP_SAMPLES="${NOOP_SAMPLES:-3}"
INC_SAMPLES="${INC_SAMPLES:-3}"
ALL_SAMPLES="${ALL_SAMPLES:-2}"

sub_ui="$here/subcrate-side/tests/ui_bench.rs"
try_case1="$here/trybuild-side/tests/ui/case001.rs"

now() { date +%s.%N; }
# elapsed START END -> seconds with 2 decimals (one per line, for stat_min_mean)
elapsed() { awk "BEGIN{printf \"%.2f\n\", $2-$1}"; }
# stat_min LABEL < samples  -> prints "min / mean" over stdin numbers
stat_min_mean() { awk '{v[NR]=$1; s+=$1} END{m=v[1]; for(i=1;i<=NR;i++) if(v[i]<m)m=v[i]; printf "%.2f / %.2f", m, s/NR}'; }

# time_pkg PKG [ENV=VAL ...] -> prints elapsed seconds for one `cargo test -p PKG`
time_pkg() {
    local pkg="$1"; shift
    local s e
    s=$(now)
    ( cd "$here" && env "$@" cargo test -p "$pkg" ) >/dev/null 2>&1
    e=$(now)
    elapsed "$s" "$e"
}

bump_one() { # rebuild just case 1 on both sides
    local c="$RANDOM$RANDOM"
    sed -i "s/MUTx[0-9]*/MUTx$c/" "$sub_ui" "$try_case1"
}
bump_all() { # rebuild every case on both sides
    local c="$RANDOM$RANDOM"
    sed -i "s/VTAG[0-9]*/VTAG$c/g" "$sub_ui"
    for f in "$here"/trybuild-side/tests/ui/case*.rs; do
        sed -i "s/VTAG[0-9]*/VTAG$c/g" "$f"
    done
}

echo "=== subcrate vs trybuild: $N compile-fail cases ==="
echo "rustc: $(rustc --version)"
echo

echo "[setup] generating $N cases"
bash "$here/gen.sh" "$N" >/dev/null

echo "[setup] warming caches + building trybuild .stderr snapshots"
( cd "$here" && cargo test -p subcrate-side ) >/dev/null 2>&1
# TRYBUILD=overwrite records each case's current stderr so later runs compare-pass.
( cd "$here" && TRYBUILD=overwrite cargo test -p trybuild-side ) >/dev/null 2>&1
( cd "$here" && cargo test -p trybuild-side ) >/dev/null 2>&1

# --- cold: from a clean target, incl. compiling each framework's own deps ---
echo "[cold] clean rebuild (includes compiling the framework itself)"
( cd "$here" && cargo clean ) >/dev/null 2>&1
COLD_SUB=$(time_pkg subcrate-side)
( cd "$here" && cargo clean ) >/dev/null 2>&1
COLD_TRY=$(time_pkg trybuild-side)

# The cold phase's second `clean` left subcrate-side unbuilt; rebuild it so the
# no-op scenario below measures a genuine no-op for both sides.
( cd "$here" && cargo test -p subcrate-side ) >/dev/null 2>&1

# --- no-op: nothing changed since last build ---
echo "[no-op] re-run with no changes ($NOOP_SAMPLES samples)"
NOOP_SUB=$(for _ in $(seq 1 "$NOOP_SAMPLES"); do time_pkg subcrate-side; done | stat_min_mean)
NOOP_TRY=$(for _ in $(seq 1 "$NOOP_SAMPLES"); do time_pkg trybuild-side; done | stat_min_mean)

# --- incremental: edit one case, re-run (deps + other cases warm) ---
echo "[one]   edit a single case, re-run ($INC_SAMPLES samples)"
INC_SUB=$(for _ in $(seq 1 "$INC_SAMPLES"); do bump_one; time_pkg subcrate-side; done | stat_min_mean)
INC_TRY=$(for _ in $(seq 1 "$INC_SAMPLES"); do bump_one; time_pkg trybuild-side; done | stat_min_mean)

# --- rebuild all: every case changed, deps warm ---
echo "[all]   rebuild every case, deps warm ($ALL_SAMPLES samples)"
ALL_SUB=$(for _ in $(seq 1 "$ALL_SAMPLES"); do bump_all; time_pkg subcrate-side; done | stat_min_mean)
ALL_TRY=$(for _ in $(seq 1 "$ALL_SAMPLES"); do bump_all; time_pkg trybuild-side; done | stat_min_mean)

echo
printf '%-34s %14s %14s\n' "scenario (seconds)" "subcrate" "trybuild"
printf '%-34s %14s %14s\n' "----------------------------------" "--------------" "--------------"
printf '%-34s %14s %14s\n' "cold (incl. framework build)"       "$COLD_SUB"  "$COLD_TRY"
printf '%-34s %14s %14s\n' "no-op re-run (min / mean)"          "$NOOP_SUB"  "$NOOP_TRY"
printf '%-34s %14s %14s\n' "one case edited (min / mean)"       "$INC_SUB"   "$INC_TRY"
printf '%-34s %14s %14s\n' "all $N cases rebuilt (min / mean)"  "$ALL_SUB"   "$ALL_TRY"
echo
echo "note: 'cold' is dominated by each framework's own dependency compile."
echo "      the no-op / one / all rows isolate the UI-test work (deps warm)."
