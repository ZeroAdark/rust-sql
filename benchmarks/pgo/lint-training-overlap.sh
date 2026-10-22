#!/usr/bin/env sh
# Fail if any PGO training statement is a published measurement statement, or
# (column-blind) a published analytical statement with its columns renamed.
# POSIX sh + awk only: runs on a laptop, in CI, and inside the build pod
# immediately before the instrumented profile pass.
#
#   usage: pgo/lint-training-overlap.sh [<training-file> ...]
#          no args = lint the checked-in corpus
#          PGO_DENYLIST_DIR=<dir>  override the denylist location
#          PGO_DENYLIST_DIR=<dir> pgo/lint-training-overlap.sh   check the corpus against
#                                  any query files you put in <dir> (*.sql, one statement per
#                                  line, with a "-- pgo-lint: column-blind" line for rule 2)
#   usage: pgo/lint-training-overlap.sh --skeletons <file> ...
#          print the column-blind skeleton of every statement (the form a
#          "-- pgo-lint: skeletons" denylist stores); reads no denylist
#   usage: pgo/lint-training-overlap.sh --exact-hashes <file> ...
#          print one "-- pgo-lint: exact-sha256 <hex>" line per distinct canonical
#          form of the statements (the exact-copy rule without the statement text)
#   usage: pgo/lint-training-overlap.sh --self-test
#          seed a scratch denylist and training files; check that the lint catches them
#
# Canonicalization and the argument for why it is sound: see
# pgo/lint-training-overlap.awk and pgo/README.md.
#
# Exit 0 = disjoint (prints a PROOF line). Exit 1 = overlap or near-copy found.
# Exit 2 = usage/IO error — never treat as a pass.
set -u
export LC_ALL=C   # bytes-as-bytes: no locale-dependent matching, no multibyte warnings

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROG="$HERE/lint-training-overlap.awk"
TAB=$(printf '\t')
if command -v sha256sum >/dev/null 2>&1; then SHA="sha256sum"; else SHA="shasum -a 256"; fi
# <file>:<line> TAB <sha256 of the canonical form>, one per statement of the given files
canon_hashes() {
  PGO_PRINT_CANON=1 awk -f "$PROG" "$@" | while IFS="$TAB" read -r where c; do
    printf '%s%s%s\n' "$(printf '%s' "$c" | $SHA | cut -c1-64)" "$TAB" "$where"
  done
}
if [ "${1:-}" = --exact-hashes ]; then
  shift
  [ "$#" -gt 0 ] || { echo "pgo-lint: FATAL --exact-hashes needs at least one file" >&2; exit 2; }
  for f in "$@"; do [ -r "$f" ] || { echo "pgo-lint: FATAL unreadable file: $f" >&2; exit 2; }; done
  canon_hashes "$@" | cut -f1 | sort -u | sed 's/^/-- pgo-lint: exact-sha256 /'
  exit 0
fi
if [ "${1:-}" = --self-test ]; then
  T=$(mktemp -d "${TMPDIR:-/tmp}/pgo-lint-selftest.XXXXXX") || exit 2
  trap 'rm -rf "$T"' EXIT
  mkdir "$T/deny"
  pub="SELECT region, sum(amount) AS total FROM sales WHERE sold_on >= DATE '1994-01-01' GROUP BY region ORDER BY total DESC;"
  printf '%s\n' "$pub" > "$T/published.sql"
  { echo "-- pgo-lint: skeletons"; sh "$0" --skeletons "$T/published.sql"; sh "$0" --exact-hashes "$T/published.sql"; } > "$T/deny/vector.sql"
  printf 'SELECT id FROM accounts WHERE id = 1;\n' > "$T/deny/oltp-official.sql"
  grep -q "region" "$T/deny/vector.sql" && { echo "pgo-lint: self-test FAIL (the skeleton+hash file holds statement text)"; exit 2; }
  renamed="SELECT area, sum(cost) AS t FROM orders WHERE placed_on >= DATE '2001-02-03' GROUP BY area ORDER BY t DESC;"
  rebound="SELECT region, sum(amount) AS total FROM sales WHERE sold_on >= DATE '2020-12-31' GROUP BY region ORDER BY total DESC;"
  other="SELECT count(*) FROM widgets WHERE w > 3 AND w < 9;"
  fails=0
  check() { # name, want rc, want text, file name, statement
    printf '%s\n' "$5" > "$T/$4"
    out=$(PGO_DENYLIST_DIR="$T/deny" sh "$0" "$T/$4" 2>&1); rc=$?
    if [ "$rc" != "$2" ] || ! printf '%s' "$out" | grep -q "$3"; then
      echo "pgo-lint: self-test FAIL ($1): rc=$rc, want $2 and '$3'"; printf '%s\n' "$out" | sed 's/^/    /'; fails=1
    fi
    rm -f "$T/$4"
  }
  check "verbatim published query in oltp-* fails (exact hash)" 1 "OVERLAP" oltp-train.sql "$pub"
  check "rebound literals in oltp-* fail (exact hash)" 1 "exact-sha256" oltp-train.sql "$rebound"
  check "verbatim published query in analytics fails" 1 "OVERLAP" analytics-train.sql "$pub"
  check "renamed columns in analytics fail (skeleton)" 1 "NEAR-COPY" analytics-train.sql "$renamed"
  check "renamed columns in oltp-* are a NOTE (skeleton, not enforced)" 0 "noted, not enforced" oltp-train.sql "$renamed"
  check "unrelated statement passes" 0 "PROOF disjoint" analytics-train.sql "$other"
  [ "$fails" = 0 ] || exit 2
  echo "pgo-lint: self-test PASS (6 cases)"
  exit 0
fi
if [ "${1:-}" = --skeletons ]; then
  shift
  [ "$#" -gt 0 ] || { echo "pgo-lint: FATAL --skeletons needs at least one file" >&2; exit 2; }
  for f in "$@"; do [ -r "$f" ] || { echo "pgo-lint: FATAL unreadable file: $f" >&2; exit 2; }; done
  PGO_PRINT_SKELETONS=1 awk -f "$PROG" "$@"
  exit $?
fi
DENYDIR="${PGO_DENYLIST_DIR:-$HERE/denylist}"
EXEMPT="$DENYDIR/EXEMPTIONS.txt"

[ -r "$PROG" ] || { echo "pgo-lint: FATAL missing $PROG" >&2; exit 2; }
[ -d "$DENYDIR" ] || { echo "pgo-lint: FATAL denylist dir missing: $DENYDIR" >&2; exit 2; }
[ -r "$EXEMPT" ] || EXEMPT=""

if [ "$#" -eq 0 ]; then
  set -- "$HERE/corpus/analytics-pulse.sql" \
         "$HERE/corpus/analytics-pulse-fixture.sql" \
         "$HERE/corpus/oltp-generic.sql" \
         "$HERE/corpus/oltp-schema.sql"
fi
for f in "$@"; do
  [ -r "$f" ] || { echo "pgo-lint: FATAL unreadable training file: $f" >&2; exit 2; }
done

deny_files=$(find "$DENYDIR" -maxdepth 1 -name '*.sql' | sort)
[ -n "$deny_files" ] || { echo "pgo-lint: FATAL no *.sql in $DENYDIR" >&2; exit 2; }

TH=$(mktemp "${TMPDIR:-/tmp}/pgo-lint-hashes.XXXXXX") || exit 2
trap 'rm -f "$TH"' EXIT
canon_hashes "$@" > "$TH" || { echo "pgo-lint: FATAL could not hash the training statements" >&2; exit 2; }
PGO_DENY_FILES="$deny_files" PGO_EXEMPT_FILE="$EXEMPT" PGO_TRAIN_HASHES="$TH" awk -f "$PROG" "$@"
