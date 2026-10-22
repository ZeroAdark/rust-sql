#!/usr/bin/env bash
# lint-alloc.sh — the std-allocation ratchet (fallible-allocation program).
# Owner ruling 2026-09-15 (docs/bug-campaign-20260913/LANE-FALLIBLE-ALLOC-REPORT.md):
# allocation failure must surface as C's ERROR 53200 "out of memory", never
# as a process abort. Precedent and shape: lint-determinism.sh (the DST P0
# ratchet) — same scanner, same allowlist grammar, same ratchet law.
#
# WHAT IT FENCES: every lexically visible std-collection allocation whose
# size is NOT a compile-time constant, in the ported crates (crates/backend,
# crates/pl, crates/contrib). Such a site allocates through the global
# allocator's infallible lane: on failure Rust calls handle_alloc_error and
# the whole server aborts, where C's palloc/malloc failure is a catchable
# ERROR 53200. Three categories, each a per-line grep over production code,
# diffed against a budgeted allowlist:
#   cap      Vec/String/VecDeque/HashMap/HashSet/BTreeMap/BinaryHeap
#            ::with_capacity(<expr>) / with_capacity_and_hasher(<expr>, ..)
#            and `vec![<elem>; <expr>]` repeat forms, when <expr> is NOT a
#            constant (see EXEMPT below)
#   reserve  .reserve( / .reserve_exact( / .resize( / .resize_with( — the
#            explicit-growth calls (name-based: a PgVec .reserve( counts too,
#            and rightly — allocator_api2's Vec::reserve is the same abort
#            lane; the sanctioned call is try_reserve). A resize/reserve on a
#            binding that was try_reserve'd / vec_with_capacity_in'd within
#            the 4 preceding code lines is a fill within reserved capacity
#            and is NOT a site (the reserved-capacity rule, see the scanner)
#   collect  collect::<Vec / collect::<String / collect::<HashMap /
#            collect::<HashSet / collect::<BTreeMap / collect::<VecDeque /
#            collect::<Box<[ and .to_vec( — whole-input materialisation of an
#            iterator or slice into a fresh std collection
#
# SANCTIONED IDIOMS (what a conversion looks like — crates/_support/mcx):
#   * allocate in a memory context: `mcx::PgVec` via `mcx::vec_with_capacity_in
#     (mcx, n)?` / `mcx::vec_new_in(mcx)` / `mcx::slice_in(mcx, src)?` /
#     `mcx::arena_vec_in(..)?` — the context's limit and the MaxAllocSize
#     ceiling are enforced by the allocator and surface as `mcx.oom(bytes)`
#     (ERROR 53200 with C's DETAIL line);
#   * grow a std or Pg collection fallibly:
#         v.try_reserve(n).map_err(|_| mcx.oom(n * size_of::<T>()))?;   (with a context)
#         v.try_reserve(n).map_err(|_| mcx::oom_named("<C context name>", bytes))?;
#     then push/extend/resize within the reserved capacity (push after a
#     successful try_reserve(1) cannot allocate);
#   * `mcx::vec_append_bytes(&mut v, bytes)?` for StringInfo-style appends.
#
# EXEMPT (lexically, never ledgered): a with_capacity / vec![_; N] argument
# that is a compile-time constant — an integer literal, an UPPER_CASE const
# (path-qualified or not), or `size_of::<T>()`, combined with + - * / << >>
# and parentheses. The rule is "no lowercase letter in the argument except
# inside size_of::<..>()": `Vec::with_capacity(64)`, `vec![0u8; BLCKSZ]`,
# `Vec::with_capacity(2 * PG_IOV_MAX)` are exempt; `Vec::with_capacity(n)`,
# `vec![0; len as usize]`, `Vec::with_capacity(64usize)` are sites (the last
# is a spelling to avoid). A bounded-constant buffer failing to allocate is
# the "process heap is gone" case that C also dies in (the stack frame would
# not have been there either); every other site is user-sized in principle.
#
# NOT LINTED (dynamic, not lexical — same reasoning as lock waits in
# lint-determinism.sh): implicit growth through push / insert / extend /
# extend_from_slice / format! / to_string / to_owned / clone / Box::new.
# Those are ubiquitous (thousands of lines) and each is a one-element step;
# the conversion discipline is that a file converted under this program
# routes its growth through try_reserve BEFORE the push loop. The lint fences
# the sizing idioms that name the user-sized quantity, which is where the
# large single allocations live (the 12 critical rows of the 2026-09-13 bug
# campaign are all of this shape).
#
# PRODUCTION CODE = crates/{backend,pl,contrib}/**/*.rs minus
# tests|test|benches|examples dirs, tests.rs/test.rs/*_test(s).rs/build.rs
# basenames, brace-matched #[cfg(test)] items, and comments — the
# lint-determinism census methodology. A "site" = one surviving source line
# matching a category pattern. crates/_support (the allocator itself and its
# shims), crates/bin, crates/interfaces are OUT of scope: they are not
# backend session code (no ERROR to raise into).
#
# THE RATCHET LAW: allowlist budgets may only shrink; rows may only be
# deleted. A new std-allocation site fails this lint. The fix is one of the
# sanctioned idioms above — or, if a reviewer deliberately accepts a new
# site, add a row carrying an "ALLOC-REVIEW(<who>): <why>" marker. Marker
# discipline on NEW/RAISED rows is enforced by HUMAN REVIEW of allowlist
# diffs. What the lint machine-enforces: malformed markers, duplicate rows,
# and budget<1 are CONFIG-FAILs (exit 2); every marker row is surfaced as a
# NOTE on every run. Rows whose sites vanished get a stale WARN: delete them.
# Rows whose sites shrank get a shrink WARN: lower the budget.
#
# ACCEPTED LIMITS (grep-family ratchet; inherited from lint-determinism):
# a site is a matched LINE; a with_capacity/reserve/resize call whose
# argument continues on the next line counts at the call line and cannot be
# constant-exempted (the argument is not visible — ledger it or join the
# line); a `vec![` whose body continues on the next line is assumed to be
# the list form (fixed element count) and is NOT counted — a multi-line
# repeat form `vec![\n elem;\n n]` evades (join it); a fallible call is
# recognised only when the `?` / .map_err follows on the SAME line or the
# first argument is a *mcx binding; the name-based patterns flag same-named
# methods on non-collection types (dshash::resize, a hash-table resize) —
# ledger those with an annotation; macro-generated sites count at the macro
# definition; `.collect()` with an inferred target type is not visible
# lexically and is not counted; spaced path tokens are not matched.
#
# Allowlist: crates/_support/seams_init/tests/lint-alloc.allow (override: LINT_ALLOC_ALLOWLIST).
# Twin: scripts/lint-alloc.{sh,allow} — patch BOTH (the seams_init copy is the enforced one).
# Scan root: the repo (override: LINT_ALLOC_TREE=<dir containing crates/>).
# Regeneration (audit/reseed under explicit charter ONLY — never routine):
#     scripts/lint-alloc.sh --regen > scripts/lint-alloc.allow
# Site listing (conversion worklist, not a gate):
#     scripts/lint-alloc.sh --sites [path-prefix]     -> "category<TAB>file:line<TAB>source"
#     scripts/lint-alloc.sh --by-crate                -> "sites<TAB>crate" (remaining work, desc)
#
# Standalone:   scripts/lint-alloc.sh            (exit 0 = clean)
# Unit-shaped:  cargo test -p seams_init --test lint_alloc
set -u

# REPO is the public workspace root (this file is public/crates/_support/seams_init/tests/*.sh);
# git rev-parse --show-toplevel would return the monorepo root, one level up.
REPO="$(cd "$(dirname "$0")/../../../.." && pwd)"
TREE="${LINT_ALLOC_TREE:-$REPO}"
ALLOWLIST="${LINT_ALLOC_ALLOWLIST:-$REPO/crates/_support/seams_init/tests/lint-alloc.allow}"
MODE="check"
PREFIX=""
case "${1:-}" in
    --regen) MODE="regen" ;;
    --sites) MODE="sites"; PREFIX="${2:-}" ;;
    --by-crate) MODE="bycrate" ;;
esac

[ -d "$TREE/crates" ] || { echo "lint-alloc: $TREE/crates not found"; exit 2; }

TMP="$(mktemp -d "${TMPDIR:-/tmp}/lint-alloc.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT INT TERM

# --- production file list (census methodology; ported-crate scope) ----------
: > "$TMP/files"
for sub in backend pl contrib; do
    [ -d "$TREE/crates/$sub" ] || continue
    find "$TREE/crates/$sub" -type f -name '*.rs' \
        ! -path '*/tests/*' ! -path '*/test/*' ! -path '*/benches/*' \
        ! -path '*/examples/*' ! -path '*/target/*' \
        ! -name 'tests.rs' ! -name 'test.rs' ! -name '*_test.rs' ! -name '*_tests.rs' \
        ! -name 'build.rs' >> "$TMP/files" || exit 2
done
LC_ALL=C sort -o "$TMP/files" "$TMP/files"

# --- scanner: one "category<TAB>relpath<TAB>line<TAB>source" record per site --
# (plus "poison<TAB>relpath" when scan state is still open at a file's EOF)
cat > "$TMP/scan.awk" <<'AWK'
function brace_delta(line,   t, d) {
    t = line
    gsub(/'([^'\\]|\\.)'/, "''", t)
    gsub(/"([^"\\]|\\.)*"/, "\"\"", t)
    d = gsub(/\{/, "", t); d -= gsub(/\}/, "", t)
    return d
}
# strip comments from one line, string/char-literal aware (lint-determinism
# scanner, verbatim). Uses/updates the global inblk.
function strip_code(line,   out, i, n, c, c2, q) {
    if (inblk) {
        q = index(line, "*/")
        if (q == 0) return ""
        line = substr(line, q + 2); inblk = 0
    }
    if (index(line, "//") == 0 && index(line, "/*") == 0) return line
    out = ""; i = 1; n = length(line)
    while (i <= n) {
        c = substr(line, i, 1)
        if (c == "\"") {
            out = out c; i++
            while (i <= n) {
                c = substr(line, i, 1)
                if (c == "\\") { out = out substr(line, i, 2); i += 2; continue }
                out = out c; i++
                if (c == "\"") break
            }
            continue
        }
        if (c == "'") {
            if (substr(line, i + 1, 1) == "\\" && substr(line, i + 3, 1) == "'") {
                out = out substr(line, i, 4); i += 4; continue
            }
            if (substr(line, i + 1, 1) != "\\" && substr(line, i + 2, 1) == "'") {
                out = out substr(line, i, 3); i += 3; continue
            }
            out = out c; i++; continue
        }
        c2 = substr(line, i, 2)
        if (c2 == "//") break
        if (c2 == "/*") {
            q = index(substr(line, i + 2), "*/")
            if (q == 0) { inblk = 1; break }
            i = i + q + 3
            continue
        }
        out = out c; i++
    }
    return out
}
function is_cfg_test(line) {
    if (line ~ /#\[[ \t]*cfg[ \t]*\([ \t]*((any|all)[ \t]*\([ \t]*)?test[^A-Za-z0-9_]/) return 1
    if (line ~ /#\[[ \t]*cfg[ \t]*\([ \t]*(any|all)[ \t]*\(/ \
        && line !~ /not[ \t]*\(/ \
        && line ~ /#\[[ \t]*cfg[ \t]*\([^]]*[(, \t]test[ \t]*[,)]/) return 1
    return 0
}
# Extract the text between the "(" at position p and its matching ")" —
# empty string when the call is not closed on this line (multi-line arg).
function paren_arg(line, p,   depth, i, n, c) {
    depth = 0; n = length(line)
    for (i = p; i <= n; i++) {
        c = substr(line, i, 1)
        if (c == "(") depth++
        else if (c == ")") { depth--; if (depth == 0) return substr(line, p + 1, i - p - 1) }
    }
    return ""
}
# A constant-size argument: no lowercase letters once size_of::<..>() /
# size_of_val-free constant forms are removed. Integer literals, UPPER_CASE
# consts (path-qualified), arithmetic and parens are constant; anything
# naming a variable, a call, a field or a cast is user-sized.
function is_const_expr(arg,   t) {
    t = arg
    gsub(/(core::|std::|mem::)*size_of::<[^>]*>\(\)/, "1", t)
    gsub(/(core::|std::|mem::)*align_of::<[^>]*>\(\)/, "1", t)
    gsub(/(::)?[a-z_][a-z0-9_]*::/, "", t)          # path segments: ::types_core::BLCKSZ
    gsub(/[0-9](u8|u16|u32|u64|usize|i8|i16|i32|i64|isize)/, "0", t)   # typed literals
    gsub(/ as (u8|u16|u32|u64|usize|i8|i16|i32|i64|isize)/, "", t)     # casts of constants
    if (t ~ /^[ \t]*$/) return 1
    if (t ~ /[a-z]/) return 0
    return 1
}
# The call is fallible when its result is immediately `?`-propagated or
# map_err'd (the mcx-backed builders: NodeList::with_capacity(mcx, n)?,
# vec_with_capacity_in(mcx, n)?) or takes a context as its first argument.
function fallible_call(rest, arg,   tail) {
    if (arg ~ /^[ \t]*&?[a-z_]*mcx[ \t]*,/) return 1
    tail = substr(rest, length(arg) + 3)
    if (tail ~ /^[ \t]*(\?|\.map_err|\.ok_or|\.unwrap_or)/) return 1
    return 0
}
# with_capacity(<arg>): count unless <arg> is constant. Handles several
# calls on one line (each counted once per line max, census parity).
function cap_site(line,   rest, p, arg, q) {
    rest = line
    while ((p = index(rest, "with_capacity")) > 0) {
        rest = substr(rest, p + length("with_capacity"))
        if (substr(rest, 1, 11) == "_and_hasher") rest = substr(rest, 12)
        if (substr(rest, 1, 1) != "(") continue
        arg = paren_arg(rest, 1)
        if (arg == "") return 1              # multi-line: not exemptable
        if (fallible_call(rest, arg)) continue
        q = index(arg, ","); if (q > 0) arg = substr(arg, 1, q - 1)
        if (!is_const_expr(arg)) return 1
    }
    return 0
}
# vec![<elem>; <expr>]: count unless <expr> is constant. The list form
# vec![a, b, c] has a fixed element count and is never a site.
function vecmacro_site(line,   rest, p, q, body, depth, i, c, semi, n) {
    rest = line
    while ((p = index(rest, "vec![")) > 0) {
        if (p > 1 && substr(rest, p - 1, 1) ~ /[A-Za-z0-9_]/) { rest = substr(rest, p + 5); continue }
        rest = substr(rest, p + 4)
        depth = 0; semi = 0; n = length(rest); body = ""
        for (i = 1; i <= n; i++) {
            c = substr(rest, i, 1)
            if (c == "[" || c == "(" || c == "{") depth++
            else if (c == "]" || c == ")" || c == "}") { depth--; if (depth == 0) { body = substr(rest, 2, i - 2); break } }
            else if (c == ";" && depth == 1) semi = i
        }
        if (body == "" && i > n) return 0     # multi-line body: assumed list form (fixed count)
        if (semi == 0) { rest = substr(rest, i + 1); continue }
        if (!is_const_expr(substr(rest, semi + 1, i - semi - 1))) return 1
        rest = substr(rest, i + 1)
    }
    return 0
}
# .reserve/.resize(<arg>, ..): count unless <arg> is constant or the call is
# `?`-propagated (a fallible resize helper).
# RESERVED-CAPACITY RULE: a .resize/.reserve whose receiver binding was
# sized fallibly within the 4 preceding code lines — `R.try_reserve[_exact](`
# / `std_vec_reserve(.., &mut R,` / `let mut R[: T] = ..vec_with_capacity_in(`
# / `..std_vec_with_capacity(` — is a fill within reserved capacity and cannot
# allocate: the sanctioned "reserve fallibly, then resize" idiom.
function within_reserved(recv,   i, prev, chain, sawcap, sawlet) {
    if (recv == "") return 0
    chain = 0; sawcap = 0; sawlet = 0
    for (i = 1; i <= 4; i++) {
        prev = hist[i]
        if (index(prev, "vec_with_capacity_in") > 0 || index(prev, "std_vec_with_capacity") > 0) sawcap = 1
        if (index(prev, "let mut " recv) > 0) sawlet = 1
        if (sawcap && sawlet) return 1
        if (index(prev, recv ".try_reserve") > 0) return 1
        # method chain split across lines: `recv` alone, then `.try_reserve..`
        if (prev ~ /^[ \t]*\.try_reserve/) chain = 1
        else if (chain && prev ~ ("(^|[^A-Za-z0-9_.])" recv "[ \t]*$")) return 1
        if (index(prev, "std_vec_reserve(") > 0 && index(prev, "&mut " recv) > 0) return 1
        if ((index(prev, "vec_with_capacity_in") > 0 || index(prev, "std_vec_with_capacity") > 0) \
            && (index(prev, "let mut " recv " ") > 0 || index(prev, "let mut " recv ":") > 0 \
                || index(prev, "let mut " recv "=") > 0 || index(prev, recv " = ") > 0)) return 1
    }
    return 0
}
function receiver_of(line, p,   head, m) {
    head = substr(line, 1, p - 1)
    if (match(head, /[A-Za-z0-9_.]+$/)) return substr(head, RSTART, RLENGTH)
    return ""
}
function reserve_site(line,   rest, ms, ml, arg, q, recv, consumed) {
    rest = line; consumed = 0
    while (match(rest, /\.(reserve|reserve_exact|resize|resize_with)[ \t]*\(/)) {
        ms = RSTART; ml = RLENGTH          # receiver_of's match() clobbers RSTART/RLENGTH
        recv = receiver_of(line, consumed + ms)
        rest = substr(rest, ms + ml - 1); consumed += ms + ml - 2
        arg = paren_arg(rest, 1)
        if (arg == "") return 1
        if (fallible_call(rest, arg)) continue
        q = index(arg, ","); if (q > 0) arg = substr(arg, 1, q - 1)
        if (is_const_expr(arg)) continue
        if (within_reserved(recv)) continue
        return 1
    }
    return 0
}
FNR == 1 {
    if (rel != "" && (inblk || skip > 0)) print "poison\t" rel
    skip = 0; pend = 0; inblk = 0; hist[1] = hist[2] = hist[3] = hist[4] = ""
    rel = substr(FILENAME, length(tree) + 1)
}
{
    line = strip_code($0)
    if (skip > 0) { skip += brace_delta(line); next }
    if (pend) {
        if (line ~ /\{/) { skip = brace_delta(line); pend = 0; if (skip <= 0) skip = 0; next }
        if (line ~ /;[ \t]*$/) { pend = 0; next }
        next
    }
    if (is_cfg_test(line)) {
        if (line ~ /\{/) { skip = brace_delta(line); if (skip <= 0) skip = 0 }
        else pend = 1
        next
    }
    src = $0; sub(/^[ \t]+/, "", src)
    if ((line ~ /with_capacity(_and_hasher)?[ \t]*\(/ && cap_site(line)) \
        || (line ~ /vec!\[/ && vecmacro_site(line)))
        print "cap\t" rel "\t" FNR "\t" src
    if (line ~ /\.(reserve|reserve_exact|resize|resize_with)[ \t]*\(/ && reserve_site(line))
        print "reserve\t" rel "\t" FNR "\t" src
    if (line ~ /collect::<[ \t]*(Vec|String|HashMap|HashSet|BTreeMap|BTreeSet|VecDeque|BinaryHeap|Box[ \t]*<[ \t]*\[)/ \
        || line ~ /\.to_vec[ \t]*\(/)
        print "collect\t" rel "\t" FNR "\t" src
    if (line !~ /^[ \t]*$/) { hist[4] = hist[3]; hist[3] = hist[2]; hist[2] = hist[1]; hist[1] = line }
}
END { if (rel != "" && (inblk || skip > 0)) print "poison\t" rel }
AWK

tr '\n' '\0' < "$TMP/files" \
    | xargs -0 awk -v tree="$TREE/" -f "$TMP/scan.awk" > "$TMP/sites"

if [ "$MODE" = "sites" ]; then
    awk -F'\t' -v pfx="$PREFIX" '$1 != "poison" && index($2, pfx) == 1 { print $1 "\t" $2 ":" $3 "\t" $4 }' "$TMP/sites" \
        | LC_ALL=C sort -t$'\t' -k2,2V
    exit 0
fi

awk -F'\t' '$1 == "poison" { print $1 "\t" $2; next } { print $1 "\t" $2 }' "$TMP/sites" \
    | LC_ALL=C sort | uniq -c \
    | awk '{ printf "%s\t%s\t%d\n", $2, $3, $1 }' \
    | LC_ALL=C sort > "$TMP/scan"

crate_of() {
    awk -F'\t' '{ crate = $2
        if (index(crate, "/src/") > 0) sub(/\/src\/.*/, "", crate)
        else sub(/\/[^\/]*$/, "", crate)
        print $0 "\t" crate }'
}

if [ "$MODE" = "bycrate" ]; then
    awk -F'\t' '$1 != "poison"' "$TMP/scan" | crate_of \
        | awk -F'\t' '{ n[$4] += $3 } END { for (c in n) printf "%d\t%s\n", n[c], c }' \
        | sort -t$'\t' -k1,1nr -k2,2
    exit 0
fi

# --- regen mode: emit a fresh allowlist body to stdout ----------------------
if [ "$MODE" = "regen" ]; then
    echo "# lint-alloc.allow — REGENERATED LEDGER (annotations lost; reseed"
    echo "# only under explicit charter — the ratchet law says rows only die)."
    echo "# Row format (tab-separated): <category>\t<file>\t<max-sites>[\t<annotation>]"
    awk -F'\t' '$1 == "poison" { printf "# SCAN-POISON: %s — scanner state still open at EOF; counts below may be incomplete\n", $2 }' "$TMP/scan"
    for cat in cap reserve collect; do
        echo ""
        echo "# ==== $cat ===="
        awk -F'\t' -v cat="$cat" '$1 == cat' "$TMP/scan" | crate_of | awk -F'\t' '
            {
                rows[NR] = $1 "\t" $2 "\t" $3; crateof[NR] = $4; csum[$4] += $3
            }
            END {
                prev = ""
                for (i = 1; i <= NR; i++) {
                    if (crateof[i] != prev) {
                        printf "# --- %s (%d sites) ---\n", crateof[i], csum[crateof[i]]
                        prev = crateof[i]
                    }
                    print rows[i]
                }
            }'
    done
    exit 0
fi

# --- allowlist parse + validation --------------------------------------------
[ -f "$ALLOWLIST" ] || { echo "lint-alloc: allowlist $ALLOWLIST not found"; exit 2; }
awk -F'\t' -v out="$TMP/allow" '
    /^[ \t]*(#|$)/ { next }
    {
        cat = $1; path = $2; budget = $3; annot = (NF >= 4 ? $4 : "")
        if (cat !~ /^(cap|reserve|collect)$/ || path == "" \
            || budget !~ /^[0-9]+$/ || budget + 0 < 1) {
            printf "CONFIG-ERROR: malformed allowlist row (%s:%d): %s\n", FILENAME, FNR, $0
            err = 1; next
        }
        key = cat "\t" path
        if (key in seen) {
            printf "CONFIG-ERROR: duplicate allowlist row for [%s] %s (%s:%d)\n", cat, path, FILENAME, FNR
            err = 1; next
        }
        seen[key] = 1
        if (annot ~ /ALLOC-REVIEW/ && annot !~ /ALLOC-REVIEW\([^)]+\):/) {
            printf "CONFIG-ERROR: malformed review marker on [%s] %s (want \"ALLOC-REVIEW(<who>): <why>\"): %s\n", cat, path, annot
            err = 1; next
        }
        print cat "\t" path "\t" budget "\t" annot > out
    }
    END { exit err }
' "$ALLOWLIST"
[ $? -eq 0 ] || { echo "lint-alloc CONFIG-FAIL (bad allowlist)"; exit 2; }
touch "$TMP/allow"

# --- diff scan vs allowlist ---------------------------------------------------
ALLOWBASE=$(basename "$ALLOWLIST")
{ sed 's/^/A\t/' "$TMP/allow"; sed 's/^/S\t/' "$TMP/scan"; } \
| awk -F'\t' -v allowbase="$ALLOWBASE" -v counts="$TMP/counts" -v summ="$TMP/summ" '
    BEGIN {
        guide["cap"]     = "size it in a memory context (mcx::vec_with_capacity_in(mcx, n)? / mcx::slice_in) or reserve fallibly (v.try_reserve(n).map_err(|_| mcx.oom(bytes))? / mcx::oom_named), or use a constant capacity"
        guide["reserve"] = "use try_reserve/try_reserve_exact + .map_err(|_| mcx.oom(bytes))? (or mcx::oom_named) and resize within the reserved capacity"
        guide["collect"] = "reserve fallibly then extend (v.try_reserve(iter.len())?; v.extend(iter)) or build into a mcx::PgVec (mcx::slice_in for slices)"
        viol = 0; warn = 0
    }
    $1 == "A" { budget[$2 "\t" $3] = $4 + 0; annot[$2 "\t" $3] = $5; next }
    $1 == "S" {
        if ($2 == "poison") { poison[$3] = 1; next }
        k = $2 "\t" $3; n[k] = $4 + 0
        scnt[$2] += $4; sfiles[$2]++
    }
    END {
        for (k in n) {
            split(k, a, "\t"); cat = a[1]; path = a[2]
            if (!(k in budget)) {
                printf "1\tVIOLATION(new-site): [%s] %s carries %d infallible std-allocation %s site(s) with no allowlist row — %s, or add an \"ALLOC-REVIEW(<who>): <why>\" row to %s.\n", \
                       cat, path, n[k], cat, guide[cat], allowbase
                viol++
            } else if (n[k] > budget[k]) {
                printf "1\tVIOLATION(ratchet): [%s] %s infallible %s sites grew %d -> %d (budgets may only shrink) — %s, or add an ALLOC-REVIEW marker to the row in %s.\n", \
                       cat, path, cat, budget[k], n[k], guide[cat], allowbase
                viol++
            } else if (n[k] < budget[k]) {
                printf "2\tWARN(shrink-budget): [%s] %s now has %d %s site(s) < budget %d — lower the budget (ratchet law).\n", \
                       cat, path, n[k], cat, budget[k]
                warn++
            }
        }
        for (k in budget) {
            split(k, a, "\t"); cat = a[1]; path = a[2]
            acnt[cat] += budget[k]; afiles[cat]++
            if (!(k in n)) {
                printf "2\tWARN(stale-allowlist): [%s] %s — no %s sites remain; delete the row from %s.\n", \
                       cat, path, cat, allowbase
                warn++
            }
            if (annot[k] ~ /ALLOC-REVIEW\(/) {
                printf "3\tNOTE(review-marker): [%s] %s — %s\n", cat, path, annot[k]
            }
        }
        for (p in poison) {
            printf "2\tWARN(scan-poison): %s — scanner state still open at EOF (unclosed block comment or unbalanced #[cfg(test)] braces); code after the poison point was NOT scanned.\n", p
            warn++
        }
        print viol "\t" warn > counts
        norder = split("cap reserve collect", ord, " ")
        for (i = 1; i <= norder; i++) {
            cat = ord[i]
            printf "%s: %d sites / %d files (allowlist %d/%d)\n", \
                   cat, scnt[cat] + 0, sfiles[cat] + 0, acnt[cat] + 0, afiles[cat] + 0 >> summ
        }
    }
' | LC_ALL=C sort | cut -f2-

viol=$(cut -f1 "$TMP/counts"); warn=$(cut -f2 "$TMP/counts")
echo "lint-alloc summary:"
sed 's/^/  /' "$TMP/summ"
if [ "$viol" -eq 0 ]; then
    echo "lint-alloc PASS (0 violations; $warn warning(s))"
    exit 0
else
    echo "lint-alloc FAIL ($viol violation(s); $warn warning(s))"
    exit 1
fi
