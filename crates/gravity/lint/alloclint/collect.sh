#!/usr/bin/env bash
# alloclint collect (box only: cargo): the detector's two raw outputs for the Gravity crates of scope.tsv, into OUT.
#   collect.sh OUT [clippy|ir|both] [crate dir ...]       (no dir: every crate of scope.tsv; a dir: that crate only)
#   clippy: ONE cargo clippy --no-deps over the crates' packages with this directory's clippy.toml (CLIPPY_CONF_DIR) and ONLY
#           the three disallowed_* lints + clone_on_ref_ptr (REFCOUNT-CLONE's type test, findings.py), FORCED (--force-warn: a file's `#![allow(clippy::all)]`, e.g. fn's generated shims,
#           cannot silence them) -> OUT/clippy.json (cargo JSON messages) + OUT/clippy.secs
#   ir:     ONE cargo build of the crates' libs at opt-level 0 with line tables, llvm-ir added to their --emit by a rustc
#           wrapper -> OUT/ir/<package>/*.ll, OUT/ir.secs (the IR leg is whole-program: run it over every crate)
# Both run with debug assertions and overflow checks OFF (the shipped release cfg; dev profile otherwise) in CARGO_TARGET_DIR.
set -euo pipefail
OUT=${1:?OUT dir}
WHAT=${2:-both}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../../.." && pwd)
shift $(( $# >= 2 ? 2 : $# ))
DIRS=() PKGS=()
while IFS=$'\t' read -r d p _; do
  case "$d" in ''|'#'*) continue ;; esac
  if [ $# -eq 0 ] || printf '%s\n' "$@" | grep -qx "$d"; then DIRS+=("$d"); PKGS+=("$p"); fi
done < "$HERE/scope.tsv"
[ ${#PKGS[@]} -gt 0 ] || { echo "collect: no crate of scope.tsv matches: $*" >&2; exit 2; }
mkdir -p "$OUT"
printf '%s\n' "${DIRS[@]}" > "$OUT/crates.txt"
export CARGO_PROFILE_DEV_DEBUG_ASSERTIONS=false CARGO_PROFILE_DEV_OVERFLOW_CHECKS=false CARGO_TERM_COLOR=never
cd "$ROOT"
if [ "$WHAT" = clippy ] || [ "$WHAT" = both ]; then
  t0=$(date +%s.%N)
  # kache would key a clippy run without the config: no wrapper for the linted crate (deps are plain checks)
  CLIPPY_CONF_DIR="$HERE" RUSTC_WRAPPER= cargo clippy $(printf -- '-p %s ' "${PKGS[@]}") --lib --no-deps --message-format=json -- \
    -A clippy::all --force-warn clippy::disallowed_types --force-warn clippy::disallowed_methods --force-warn clippy::disallowed_macros --force-warn clippy::clone_on_ref_ptr \
    > "$OUT/clippy.json" 2> "$OUT/clippy.err" || { echo "collect: clippy failed rc=$?" >&2; tail -20 "$OUT/clippy.err" >&2; exit 2; }
  # a crate cargo found fresh replays the messages of its last check; when that check ran it as a plain dependency of
  # another selected crate (--no-deps: not linted), the replay is empty (MEASURED 10-14: deps after decompose). A selected
  # crate with clippy rows in its ledger and no finding is re-linted once with its lib.rs touched. A finding = a disallowed_* message
  # (the replay keeps rustc's own warnings: driver's made a file_name-only test miss the empty replay, MEASURED 10-14).
  # A crate with NO ledger directory (new in scope: the seed is being cut) is re-linted the same way: its empty replay would
  # seed it with the IR leg's rows only (parquet_read, alloclint#3286: every clippy row read budget 0, main 2026-10-14T21:51:02Z).
  # A crate's findings are the messages whose TARGET is the crate: a span's file_name can name another crate's macro definition
  # (an expansion span), and a file_name test let six empty replays through (ir, tpde, pgrc_batch / format / meta / read:
  # every clippy row STALE and its sites NEW ir-only, MEASURED 2026-10-14 after an exec + fn leg built them as plain deps).
  linted=$(python3 -c '
import json, sys
seen = set()
for l in open(sys.argv[1]):
    try:
        m = json.loads(l)
    except ValueError:
        continue
    if m.get("reason") == "compiler-message" and ((m["message"].get("code") or {}).get("code") or "").startswith("clippy::disallowed_"):
        seen.add(m.get("target", {}).get("name", ""))
print(" ".join(sorted(seen)))' "$OUT/clippy.json")
  # (no pipe in the test: under pipefail `cat <ledger files> | grep -q` is 141 once grep quits at the first file's match and
  # cat writes the next file, so every multi-file ledger read "no clippy rows" and its empty replay went unseen: pgrc_batch /
  # format / meta / read, ir, tpde at the r3d precheck, MEASURED 2026-10-14T22:22Z)
  redo=()
  for i in "${!DIRS[@]}"; do
    k=$(echo "${DIRS[$i]#crates/gravity/}" | tr / .)
    t=$(echo "${PKGS[$i]}" | tr - _)
    if { [ ! -d "$ROOT/crates/gravity/lint/ALLOCLINT-DEBT/$k" ] \
         || grep -qvE '^(path|ir-only)'$'\t' "$ROOT/crates/gravity/lint/ALLOCLINT-DEBT/$k"/* 2>/dev/null; } \
       && [[ " $linted " != *" $t "* ]]; then
      touch "$ROOT/${DIRS[$i]}/src/lib.rs"; redo+=("${PKGS[$i]}")
    fi
  done
  if [ ${#redo[@]} -gt 0 ]; then
    echo "collect: re-lint (empty replay): ${redo[*]}" >&2
    mv "$OUT/clippy.json" "$OUT/clippy.run1.json"
    CLIPPY_CONF_DIR="$HERE" RUSTC_WRAPPER= cargo clippy $(printf -- '-p %s ' "${PKGS[@]}") --lib --no-deps --message-format=json -- \
      -A clippy::all --force-warn clippy::disallowed_types --force-warn clippy::disallowed_methods --force-warn clippy::disallowed_macros --force-warn clippy::clone_on_ref_ptr \
      > "$OUT/clippy.json" 2> "$OUT/clippy.err" || { echo "collect: clippy failed rc=$?" >&2; tail -20 "$OUT/clippy.err" >&2; exit 2; }
    # the re-lint run rebuilds the touched crates' dependents, and one rebuilt as a plain dependency replays nothing the next
    # time; a crate with findings in the first run and none in the second keeps the first run's (the re-cut at r3d lost six
    # crates' findings so, MEASURED 2026-10-14). The first run's copy stays beside it (clippy.run1.json).
    python3 - "$OUT/clippy.run1.json" "$OUT/clippy.json" <<'PY' >&2
import json, sys
def by_target(path):
    out, other = {}, []
    for l in open(path):
        try:
            m = json.loads(l)
        except ValueError:
            other.append(l)
            continue
        if m.get("reason") == "compiler-message":
            out.setdefault(m.get("target", {}).get("name", ""), []).append(l)
        else:
            other.append(l)
    return out, other
def finds(lines):
    return any("clippy::disallowed_" in l for l in lines)
r1, _ = by_target(sys.argv[1])
r2, other = by_target(sys.argv[2])
kept = sorted(t for t, ls in r1.items() if finds(ls) and not finds(r2.get(t, [])))
for t in kept:
    r2[t] = r1[t]
with open(sys.argv[2], "w") as f:
    f.writelines(other + [l for ls in r2.values() for l in ls])
if kept:
    print("collect: kept the first run's findings of %s (none in the re-lint run)" % " ".join(kept))
PY
  fi
  echo "$(date +%s.%N) $t0" | awk '{printf "%.1f\n", $1-$2}' > "$OUT/clippy.secs"
fi
if [ "$WHAT" = ir ] || [ "$WHAT" = both ]; then
  # ONE cargo build of every crate's lib, so each crate's IR is the build its dependents link against: per-crate
  # `cargo rustc -p X` builds resolve features per selection, a shared dependency's -C metadata moves, and a crate's
  # symbols then differ by hash between its own .ll and its callers' (MEASURED 10-14: 2,231 of 3,103 cross-crate
  # references missed). A rustc wrapper adds llvm-ir to --emit for the scope's crates only; every other crate goes
  # through the caller's wrapper (kache) unchanged.
  t0=$(date +%s.%N)
  rm -rf "$OUT/ir" "$OUT/ir.log"; mkdir -p "$OUT/ir"
  DEPS="${CARGO_TARGET_DIR:-target}/debug/deps"
  WRAP="$OUT/ir-wrapper.sh"
  cat > "$WRAP" <<'WEOF'
#!/usr/bin/env bash
crate="" prev=""
for x in "$@"; do [ "$prev" = "--crate-name" ] && crate=$x; prev=$x; done
if [ -n "$crate" ] && [[ " $ALLOCLINT_IR_CRATES " == *" $crate "* ]]; then
  args=()
  for x in "$@"; do case "$x" in --emit=*link*) x="$x,llvm-ir" ;; esac; args+=("$x"); done
  exec "${args[@]}"
fi
if [ -n "${ALLOCLINT_INNER_WRAPPER:-}" ]; then exec "$ALLOCLINT_INNER_WRAPPER" "$@"; fi
exec "$@"
WEOF
  chmod +x "$WRAP"
  for i in "${!PKGS[@]}"; do
    find "$DEPS" -maxdepth 1 -name "${PKGS[$i]}-*.ll" -delete 2>/dev/null || true
    touch "$ROOT/${DIRS[$i]}/src/lib.rs"     # rebuild the scope's crates (and only them) under the wrapper
  done
  ALLOCLINT_IR_CRATES="${PKGS[*]}" ALLOCLINT_INNER_WRAPPER="${RUSTC_WRAPPER:-}" RUSTC_WRAPPER="$WRAP" \
    cargo build $(printf -- '-p %s ' "${PKGS[@]}") --lib > "$OUT/ir.log" 2>&1 \
    || { echo "collect: ir build failed" >&2; tail -20 "$OUT/ir.log" >&2; exit 2; }
  for p in "${PKGS[@]}"; do
    mkdir -p "$OUT/ir/$p"
    find "$DEPS" -maxdepth 1 -name "${p}-*.ll" -exec mv {} "$OUT/ir/$p/" \;
    [ -n "$(ls "$OUT/ir/$p")" ] || { echo "collect: no .ll produced for $p" >&2; exit 2; }
  done
  echo "$(date +%s.%N) $t0" | awk '{printf "%.1f\n", $1-$2}' > "$OUT/ir.secs"
  find "$OUT/ir" -name '*.ll' | wc -l > "$OUT/ir.count"
fi
echo "collect: done $WHAT -> $OUT"
