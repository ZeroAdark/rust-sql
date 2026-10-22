#!/usr/bin/env bash
# alloclint ratchet runner (box only: cargo). Runs the detector on Gravity crates (scope.tsv) and holds each to its ledger
# crates/gravity/lint/ALLOCLINT-DEBT/<crate>/ (ratchet.py; reports/ALLOCLINT-GRAVITY-20261014.md section 5).
#   run.sh OUT [clippy|full] [crate dir ...]          (no dir: every crate of scope.tsv)
#   clippy (default): collect.sh clippy + ratchet.py check                      (the leg for a path-triggered check)
#   full:             + collect.sh ir of EVERY crate (the call graph is whole-program) + crosscheck.py + check of the IR-only
#                     rows of the named crates (the IR leg: what clippy cannot name), judged with the IR's own NOALLOC proofs
#                     (x/noalloc.tsv; a ledger .noalloc row the IR no longer proves prints NOALLOC-UNPROVEN). Rewrite the
#                     ledger from a full run: ratchet.py write --findings OUT/clippy.json --ir-misses OUT/x/misses.tsv
#                     --noalloc OUT/x/noalloc.tsv (a commit trailer `Alloclint-Noalloc: <crate dir>` names a crate whose
#                     .noalloc rows it adds)
# Exit: 0 PASS, 1 FAIL (a new or stale site), 2 cannot judge (build failure, no findings, no ledger).
set -euo pipefail
OUT=${1:?OUT dir}
MODE=${2:-clippy}
shift $(( $# >= 2 ? 2 : $# ))
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../../.." && pwd)
CR=()
for c in "$@"; do CR+=(--crate "$c"); done
"$HERE/collect.sh" "$OUT" clippy "$@" >&2 || exit 2
if [ "$MODE" = full ]; then
  "$HERE/collect.sh" "$OUT" ir >&2 || exit 2
  t0=$(date +%s.%N)
  python3 "$HERE/crosscheck.py" --root "$ROOT" --ll "$OUT/ir" --clippy "$OUT/clippy.json" --out "$OUT/x" > "$OUT/crosscheck.log" 2>&1 \
    || { tail -5 "$OUT/crosscheck.log" >&2; exit 2; }
  echo "$(date +%s.%N) $t0" | awk '{printf "%.1f\n", $1-$2}' > "$OUT/crosscheck.secs"
  exec python3 "$HERE/ratchet.py" check --root "$ROOT" --findings "$OUT/clippy.json" --ir-misses "$OUT/x/misses.tsv" \
    --noalloc "$OUT/x/noalloc.tsv" "${CR[@]}"
fi
exec python3 "$HERE/ratchet.py" check --root "$ROOT" --findings "$OUT/clippy.json" "${CR[@]}"
