#!/usr/bin/env python3
"""alloclint's ratchet (lane alloclint; round 1 reports/ALLOCLINT-LOWERV4-20261014.md section 4, round 2 (every Gravity
crate) reports/ALLOCLINT-GRAVITY-20261014.md section 5): the untracked allocation sites of each Gravity crate may only go down.

    ratchet.py check --findings OUT/clippy.json [--ir-misses OUT/x/misses.tsv] [--crate DIR ..] [--root R]   (box: collect.sh's output)
    ratchet.py write --findings OUT/clippy.json [--ir-misses ..] [--crate DIR ..] [--root R]   rewrite those crates' ledgers
    ratchet.py base --base REV [--tree REV] [--repo R]                                      ledger-only shrink check (any host)
    ratchet.py selftest

The ledger is lint/ALLOCLINT-DEBT/<crate>/ (shared lists are directories; <crate> = the crate's dir under crates/gravity with
'/' as '.', e.g. lowerv4, backend.interp): one file per source file, named by its path with '/' as '.' plus '.tsv', each row
`kind<TAB>item<TAB>count` (kind type | method | growth | macro from the detector, ir-only from the IR cross-check's misses).
A finding inside a home (homes.tsv: H1-H5) is tracked and is not counted. --crate (repeatable) limits check / write to those
crates (default: every crate of scope.tsv); a crate whose ledger directory is absent CANNOT be judged.
THE LAW (lint-alloc.sh's): `check` FAILS on a (file, kind, item) whose count passes its row (a new site: route it through a
home) and on a row whose count is below its budget (a removed site: lower the row, `write`). `base` FAILS when the tree's
ledger adds a row or raises a budget against REV (a file git sees renamed keeps its rows when they do not rise), and when
homes.tsv gains a row that no commit of the range names in an `Alloclint-Home: <file> <function>` trailer (a new home is a
reviewed exemption, never a quiet one). Exit 0 PASS, 1 FAIL, 2 cannot judge (a compile error, no findings, no ledger).
A SAME-SITE RENAME (main 2026-10-14T22:10:42Z): a renames.tsv row added in the range lets the new item rise by its count at
that site (base: the line is a changed line whose removed twin is it with the old item's name; the old item falls by at least
the count; the file's total does not rise). `check` with ALLOCLINT_BASE (the S1 leg's merge-base) prints each row the range
adds and FAILs one whose new item has no finding on its line.
A USER-APPROVED ADD (the user's option A on planprod's P6-c2 rows, via main 2026-10-15T14:56:41Z): a commit trailer of the range
`Alloclint-Approved: <file> <kind> <item> <count> USER-DECISIONS-<file>.md@<ts>` lets that row be added or raised by at most
<count>; the citation of the user's decision is the record (main relays the ruling; a lane never approves its own rows).
The ITEM MAY HOLD SPACES (memacct 2026-10-16 ~23:3xZ; IR items such as `<T as core::convert::Into<U>>::into`): the trailer's
words are <file> = 1st, <kind> = 2nd, the citation = last, <count> = second-to-last, <item> = every word between, joined by
single spaces (approved_fields; the tools' approved_check.py splits the same way). A 6-word trailer reads as before.
COLD-TEXT (USER RULING via main 2026-10-14T18:52:43Z): a finding on a line whose tag is a plain `// alloc: Cold` and whose
item is text (homes.cold_class: format!, String, to_string, str::*, CString) is error / refusal / log text, a NAMED class:
no row, out of the debt total, counted in its own column (check / write print it; a full `write` records it per crate in
ALLOCLINT-DEBT/<crate>/.columns, never judged). A plain Cold tag on a non-text item stays debt and is counted MISTAGGED (a
count in the tail line and .columns, no line per site: main 19:04:45Z); on an item that is text only for some receivers
(to_owned, clone, join, collect, into) it stays debt as COLD-UNPROVEN.
TYPE-MENTION (USER RULING via main 2026-10-14T23:20:10Z, TD-197 amendment): a `type` finding on a line with no method / growth /
macro finding (findings.is_mention, on the raw findings) names a type and allocates nothing: no row, never NEW, out of the debt
total, counted in its own column right after the debt (check / write print it; .columns records it per crate). A type finding
on a line that also allocates stays debt beside that line's rows. COLD-TEXT goes first (a Cold String field stays COLD-TEXT);
a Cold-tagged type-only line is a TYPE-MENTION, not MISTAGGED (MISTAGGED counts debt).
MCX-TRANSMUTE (USER APPROVED via main 2026-10-15T00:10:23Z; mcxt.py): ALLOCLINT-DEBT/mcx-transmute/ holds the lifetime-extended
context handles (kind mcx-transmute, item transmute: memlead's K1) of every crate outside crates/_support/mcx. `base` judges
them on every range: each changed file's sites (homes out) must EQUAL its rows (FAIL MCX NEW / STALE), rows are never added
or raised; the range that brings the directory in seeds it under `Alloclint-Seed: mcx-transmute`, a full scan equal to it.
CRATE RENAMES (USER RULING 2026-10-15 ~20:0xZ via memacct, the pgrc -> pgrc merge: "Carry trunk's allowances"): `base` reads
the tree's comment-budget-renames.tsv (`<new crate dir><TAB><old crate dir>`, # comments: the ONE map comment-ratchet.py and
alloc_ratchet.py read too). A row of a file under a mapped new dir that git does not pair with a base file keeps the old
path's row: the same count passes, a higher one RAISES, no old row (or the old row still in the tree) ADDS. A file whose old
path was in scope is no crate new in scope (no seed, an Alloclint-Scope trailer opens nothing), and a homes.tsv row whose old
path's row the tree drops is the same home (no trailer). Each carried crate is printed.
"""
import argparse
import collections
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import findings as findings_mod  # noqa: E402
import homes as homes_mod  # noqa: E402
import mcxt  # noqa: E402
import unreached as unreached_mod  # noqa: E402

# the named classes: no row, never NEW, out of the debt total, each in its own column
NAMED = ("COLD-TEXT", "TYPE-MENTION", "REFCOUNT-CLONE", "REFCOUNT-CLONE-INLINE", "UNREACHED", "NOALLOC", "HOMED")
NOALLOC_FILE = ".noalloc"   # ALLOCLINT-DEBT/<crate>/.noalloc: the IR-proven NOALLOC findings (findings.NOALLOC_ITEMS)
CRATE = "crates/gravity/lowerv4"
LEDGER = "crates/gravity/lint/ALLOCLINT-DEBT"
HOMES = "crates/gravity/lint/alloclint/homes.tsv"
RENAMES = "crates/gravity/lint/alloclint/renames.tsv"
CRATE_RENAMES = "comment-budget-renames.tsv"   # the tree root's crate-rename map (comment-ratchet.py, alloc_ratchet.py)
CC_PER_LINE = 2   # D4505 seed: the most findings one `charged caller` line carries (its call + the type / copy beside it)
# LEDGER_DOC: the ledger's description. Until 2026-10-15 every `write` also wrote it as ALLOCLINT-DEBT/README. That file
# was generated, so lanes on different bases wrote different texts, and it collided in merges (cvfan-st#3338's only shared file
# with the port; TD-203, main GO 2026-10-15T04:08:23Z). It now lives here and in LOWER-A-ROW.md only. Readers still skip a
# README a tree on an older base carries.
LEDGER_DOC = """# alloclint's ledger (crates/gravity/lint/alloclint/ratchet.py; reports/ALLOCLINT-GRAVITY-20261014.md): the untracked
# allocation sites of each Gravity crate on the seeding trunk, the debt budget. One directory per crate (its dir under
# crates/gravity, '/' as '.'; a server crate outside it: its whole dir, crates.backend.executor.spillset), one file per source file (its path with '/' as '.', plus '.tsv'), one row `kind<TAB>item<TAB>count` per
# detector item. A count only goes down: a new site goes through a home (a driver region / pool block via Charge, an mcx
# context, the program arena, a cache holder or cache scope, an operator arena), a removed site lowers its row
# (ratchet.py write). This text is not a row.
# COLD-TEXT (USER RULING 2026-10-14T18:52:43Z): a site tagged `// alloc: Cold` whose value is error / refusal / log text
# (format!, String, to_string, CString, ...) is a named class, not debt: it has no row. Each crate's .columns (written by a
# full `ratchet.py write`, never judged) counts it in its own column beside the debt. A Cold tag on a Vec, Box, map, ... stays
# debt (MISTAGGED).
# TYPE-MENTION (USER RULING via main 2026-10-14T23:20:10Z): a type named (Vec, HashMap, Arc, ...) on a line with no method /
# growth / macro finding allocates nothing: no row, not debt, counted in .columns' type-mention column. A type on a line that
# allocates keeps its row beside the line's allocation rows.
# mcx-transmute/ (USER APPROVED via main 2026-10-15T00:10:23Z; mcxt.py): a lifetime-extended context handle (a transmute of an
# Mcx: memlead's K1) per source file of every crate outside crates/_support/mcx, backend included: debt
# rows that only go down; ratchet.py base makes each changed file's sites equal its rows.
"""


def scope_dirs():
    out = []
    for ln in open(os.path.join(HERE, "scope.tsv")):
        if ln.startswith("#") or not ln.strip():
            continue
        out.append(ln.split("\t")[0].rstrip("/"))
    return out


SEED_IR_ONLY_MIN = 20   # a crate this size with IR rows only is an empty replay (gravity/dict, 1 ir-only row, is a real one)


def crate_key(d):
    """lint/ALLOCLINT-DEBT/<key>: the crate dir under crates/gravity with '/' as '.'"""
    return d[len("crates/gravity/"):].replace("/", ".") if d.startswith("crates/gravity/") else d.replace("/", ".")


def ledger_name(path):
    """the ledger file of a source file: its path with '/' as '.', plus '.tsv' (a ledger file must not end in '.rs': the
    tree's .rs scanners (lint-determinism, 2026-10-14 precheck e13c871c8b2) read its item names as raw sites)"""
    return path.replace("/", ".") + ".tsv"


def read_ledger_dir(d, crates=None):
    """every row under the ledger (crate directories; a crate list limits it to those). None: no ledger at all."""
    rows = collections.Counter()
    if not os.path.isdir(d):
        return None
    for name in sorted(os.listdir(d)):
        p = os.path.join(d, name)
        if name == "README" or name.startswith("."):
            continue
        if os.path.isdir(p):
            if crates is not None and name not in crates:
                continue
            for n2 in sorted(os.listdir(p)):
                if not n2.startswith("."):
                    rows.update(parse_rows(n2, open(os.path.join(p, n2)).read()))
        elif crates is None:
            rows.update(parse_rows(name, open(p).read()))
    return rows


def parse_rows(name, text):
    """`name` is the ledger file (the source path with '/' as '.'): its rows keyed (source path, kind, item)."""
    path = None
    out = collections.Counter()
    for ln in text.splitlines():
        if not ln.strip() or ln.startswith("#"):
            continue
        c = ln.split("\t")
        if c[0] == "path":
            path = c[1]
            continue
        if len(c) != 3 or not c[2].isdigit():
            raise ValueError(f"{name}: bad row {ln!r}")
        out[(path or name, c[0], c[1])] += int(c[2])
    return out


def parse_noalloc(text):
    """{(file, line, kind, item): (count, text key, class)} of a .noalloc / x/noalloc.tsv text; class NOALLOC (the IR proves
    no allocation) or COLD-TEXT (the IR proves a Cold-tagged Box::new boxes a PgError: an error payload)"""
    out = {}
    for ln in text.splitlines():
        c = ln.split("\t")
        if ln.startswith("#") or len(c) < 6 or c[0] == "file" or not c[1].isdigit() or not c[4].isdigit():
            continue
        out[(c[0], int(c[1]), c[2], c[3])] = (int(c[4]), c[5], c[6] if len(c) > 6 and c[6] else "NOALLOC")
    return out


def noalloc_by_text(rows):
    """{(file, kind, item, text key, class): count} of .noalloc rows: a row is its line's TEXT, not its line number (a landing
    that moves an unchanged line keeps its proof; an edited line's text is another key, so it is debt again)"""
    out = collections.Counter()
    for (f, _l, k, it), (cnt, tk, cls) in rows.items():
        out[(f, k, it, tk, cls)] += cnt
    return out


def read_noalloc(root, crates):
    """the ledger's .noalloc rows of these crates"""
    out = {}
    for c in crates:
        p = os.path.join(root, LEDGER, crate_key(c), NOALLOC_FILE)
        if os.path.exists(p):
            out.update(parse_noalloc(open(p).read()))
    return out


def unreached_spans(root):
    """{file: [(first line, last line)]} of the functions unreached.tsv lists (the function rows' proof is ratchet.py base's);
    a FILE row (`*`, memacct 2026-10-16T05:0xZ) covers its whole file only while its file proof holds, re-run here on every
    lint run: when it fails, the file's findings are debt on this tree and the reasons are printed"""
    p = os.path.join(root, unreached_mod.LIST)
    rows = unreached_mod.read_rows(open(p).read()) if os.path.exists(p) else []
    want = collections.defaultdict(set)
    out = {}
    file_rows = [(f, note) for f, fn, note in rows if fn == "*"]
    if file_rows:
        tr = unreached_mod.Tree(root, None)
        res = unreached_mod.prove(tr, [(f, "*") for f, _ in file_rows])
        for f, note in file_rows:
            why = ([] if unreached_mod.FILE_PROOF in note else
                   [f"the row's note does not name {unreached_mod.FILE_PROOF!r} and its exclusions"]) + res[(f, "*")]
            if why:
                print(f"alloclint UNREACHED-FILE NOT PROVEN {f}: its findings count as debt here ({len(why)} reason(s)): "
                      + " | ".join(why[:3]))
            else:
                print(f"alloclint UNREACHED-FILE {f}: proven ({unreached_mod.FILE_PROOF}): its findings are UNREACHED")
                out[f] = [(1, 1 << 30)]
    for f, fn, _ in rows:
        if fn != "*":
            want[f].add(fn)
    for f, fns in want.items():
        full = os.path.join(root, f)
        if not os.path.exists(full):
            continue
        for row in homes_mod.fn_spans(open(full, errors="replace").read()):
            name, selfty, a_, b_ = row[:4]
            if name in fns or (selfty and f"{selfty}::{name}" in fns):
                out.setdefault(f, []).append((a_, b_))
    return out


def current(findings_path, ir_misses, crates, root=".", noalloc=None):
    errs = findings_mod.compiler_errors(findings_path)
    if errs:
        print("alloclint: CANNOT JUDGE: the detector's build failed:\n" + errs[0][:2000])
        sys.exit(2)
    homes = homes_mod.Homes(root, os.path.join(root, HOMES))
    # cold: (file, line, kind, item, COLD-TEXT | TYPE-MENTION | COLD-UNPROVEN | MISTAGGED); the first two are no site
    found, cold = [], []
    raw = findings_mod.parse(findings_path, crates)
    ends = findings_mod.refptr_spans(findings_path)
    refc = findings_mod.refcount_findings(raw, ends)
    rc_all = findings_mod.refcount_clones(raw, ends)   # REFCOUNT-CLONE-INLINE: a resolved refcount clone allocates nothing
    # UNREACHED (TD-197 amendment, memacct GO 2026-10-15T20:3xZ): a finding inside a function unreached.tsv lists (its proof:
    # unreached.py, re-run by ratchet.py base on every range)
    unr = unreached_spans(root)
    in_unr = lambda f, l: any(a_ <= l <= b_ for a_, b_ in unr.get(f, ()))  # noqa: E731
    # NOALLOC (TD-197 LINT-FP): a Clone / ToOwned / Extend finding the IR proved allocates nothing, while its line's text is
    # the one the proof saw (noalloc = the IR leg's own x/noalloc.tsv in a full run, else the ledger's .noalloc rows)
    na = read_noalloc(root, crates) if noalloc is None else noalloc
    na_left = noalloc_by_text(na)
    na_files = {k[0] for k in na_left}
    na_set, err_set = {}, set()
    src = {}
    for x in raw:
        if x[0] not in na_files or homes.of(x[0], x[1]):
            continue
        if x[0] not in src:
            try:
                src[x[0]] = open(os.path.join(root, x[0]), errors="replace").read().split("\n")
            except OSError:
                src[x[0]] = []
        ls = src[x[0]]
        if not 0 < x[1] <= len(ls):
            continue
        tk = findings_mod.line_key(ls[x[1] - 1])
        for cls in ("NOALLOC", "HOMED", "COLD-TEXT"):
            k = (x[0], x[3], x[4], tk, cls)
            if na_left.get(k, 0) > 0:
                na_left[k] -= 1
                if cls == "COLD-TEXT":
                    err_set.add(x)
                else:
                    na_set[x] = cls
                break
    gone = {x for x in raw if x in na_set or in_unr(x[0], x[1])}
    alloc_at = findings_mod.alloc_lines([x for x in raw if x not in rc_all and x not in gone])
    for x in raw:
        if homes.of(x[0], x[1]):
            continue
        if in_unr(x[0], x[1]):
            cold.append((x[0], x[1], x[3], x[4], "UNREACHED"))
            continue
        if x in na_set:
            cold.append((x[0], x[1], x[3], x[4], na_set[x]))
            continue
        cc = homes.cold(x[0], x[1], x[4])
        if cc == "MISTAGGED" and x in err_set:
            cc = "COLD-TEXT"   # a Cold-tagged Box::new the IR proves boxes a PgError: an error payload (crosscheck.py)
        if cc != "COLD-TEXT" and findings_mod.is_mention(x, alloc_at):
            cc = "TYPE-MENTION"   # (a Cold tag on a type-only line is no MISTAGGED debt: the line allocates nothing)
        if x in refc:
            cc = "REFCOUNT-CLONE"   # (the compiler's type: a count bump; a Cold tag on it is no COLD-UNPROVEN debt)
        elif x in rc_all:
            cc = "REFCOUNT-CLONE-INLINE"   # (the same count bump beside other findings: they stay debt)
        if cc:
            cold.append((x[0], x[1], x[3], x[4], cc))
        if cc not in NAMED:
            found.append(x)
    cur = collections.Counter((f, k, it) for f, l, c, k, it in found)
    lines = collections.defaultdict(list)
    for f, l, c, k, it in found:
        lines[(f, k, it)].append(l)
    if ir_misses:
        for ln in open(ir_misses).read().splitlines()[1:]:
            c = ln.split("\t")
            if len(c) >= 5 and findings_mod.crate_dir(c[0], crates):
                if in_unr(c[0], int(c[1])):
                    cold.append((c[0], int(c[1]), "ir-only", c[4], "UNREACHED"))
                    continue
                cc = homes.cold(c[0], int(c[1]), c[4])
                if cc:
                    cold.append((c[0], int(c[1]), "ir-only", c[4], cc))
                if cc == "COLD-TEXT":
                    continue
                cur[(c[0], "ir-only", c[4])] += 1
                lines[(c[0], "ir-only", c[4])].append(int(c[1]))
    # (n counts every clippy finding the crates hold, so a crate whose findings are all TYPE-MENTION / COLD-TEXT is judged)
    return cur, lines, len(found) + sum(1 for x in cold if x[4] in NAMED and x[2] != "ir-only"), cold


def cold_columns(cold):
    """the COLD-TEXT columns: (COLD-TEXT, COLD-UNPROVEN, MISTAGGED) site counts"""
    n = collections.Counter(x[4] for x in cold)
    return n["COLD-TEXT"], n["COLD-UNPROVEN"], n["MISTAGGED"]


def refcounts(cold):
    """the REFCOUNT-CLONE column: Rc / Arc / Weak clones on lines with no other finding (no row, not debt)"""
    return sum(1 for x in cold if x[4] == "REFCOUNT-CLONE")


def refcounts_inline(cold):
    """the REFCOUNT-CLONE-INLINE column: Rc / Arc / Weak clones beside other findings on their line (no row, not debt)"""
    return sum(1 for x in cold if x[4] == "REFCOUNT-CLONE-INLINE")


def mentions(cold):
    """the TYPE-MENTION column: type findings on lines with no allocation finding (no row, not debt)"""
    return sum(1 for x in cold if x[4] == "TYPE-MENTION")


def named_count(cold, cls):
    return sum(1 for x in cold if x[4] == cls)


def cold_say(cold):
    # TYPE-MENTION first: it stands beside the debt in every record line (main 2026-10-14T23:20:10Z)
    t, u, m = cold_columns(cold)
    return (f"TYPE-MENTION {mentions(cold)} (a type named on a line that allocates nothing: not debt); "
            f"NOALLOC {named_count(cold, 'NOALLOC')} (a Clone / ToOwned / Extend / collect the IR proved allocates nothing: not debt); "
            f"HOMED {named_count(cold, 'HOMED')} (one the IR proved allocates only through a home, an ArenaVec / PgVec copy: not debt); "
            f"UNREACHED {named_count(cold, 'UNREACHED')} (in a function only test code reaches, unreached.tsv: not debt); "
            f"REFCOUNT-CLONE {refcounts(cold)} (an Rc / Arc / Weak clone alone on its line: not debt); "
            f"REFCOUNT-CLONE-INLINE {refcounts_inline(cold)} (one beside other findings: not debt, they are); "
            f"COLD-TEXT {t} (named class, not debt); COLD-UNPROVEN {u} and MISTAGGED {m} (still debt)")


def crates_of(a):
    return [c.rstrip("/") for c in a.crate] if a.crate else scope_dirs()


def cmd_check(a):
    crates = crates_of(a)
    fresh = parse_noalloc(open(a.noalloc).read()) if getattr(a, "noalloc", None) else None
    cur, lines, n, cold = current(a.findings, a.ir_misses, crates, a.root, fresh)
    if fresh is not None:
        # the IR leg's own view judges this tree; a ledger .noalloc row it no longer proves is printed (S3-slow alloclint-ir
        # reports it: report mode until 3 clean runs on different heads, then keyed RED; memacct 2026-10-15T20:3xZ)
        led_rows = read_noalloc(a.root, crates)
        fr_t, at_line = noalloc_by_text(fresh), {}
        for (f, l, k, it), (cnt, tk, cls) in led_rows.items():
            at_line.setdefault((f, k, it, tk, cls), l)
        for key, cnt in sorted(noalloc_by_text(led_rows).items()):
            if fr_t.get(key, 0) < cnt:
                print(f"alloclint NOALLOC-UNPROVEN {key[0]}:{at_line[key]} {key[1]} {key[2]}: the ledger's .noalloc says {cnt}, the IR now "
                      f"proves {fr_t.get(key, 0)} (rewrite it: run.sh full + ratchet.py write --noalloc)")
    keys = {crate_key(c) for c in crates}
    if not os.path.isdir(os.path.join(a.root, LEDGER)):
        print(f"alloclint: CANNOT JUDGE: no ledger at {LEDGER}")
        return 2
    absent = sorted(k for k in keys if not os.path.isdir(os.path.join(a.root, LEDGER, k)))
    if absent:
        print(f"alloclint: CANNOT JUDGE: no ledger directory for {', '.join(absent)} under {LEDGER} (seed it: ratchet.py write --crate ..)")
        return 2
    led = read_ledger_dir(os.path.join(a.root, LEDGER), keys)
    if n == 0 and any(k[1] != "ir-only" for k in led):
        # (a crate whose only rows are the IR leg's, e.g. dict's one ir-only row, has no clippy finding to expect)
        print("alloclint: CANNOT JUDGE: the detector reported no finding at all (was CLIPPY_CONF_DIR read?)")
        return 2
    fails = []
    for key in sorted(cur):
        if cur[key] > led.get(key, 0):
            f, k, it = key
            fails.append(f"alloclint FAIL NEW {f}:{','.join(map(str, sorted(lines[key])))} {k} {it}: {cur[key]} site(s), budget "
                         f"{led.get(key, 0)}: route it through a home (an mcx context, a driver region / pool block, the program "
                         f"arena, a cache holder or scope, an operator arena) instead of the global heap "
                         f"(crates/gravity/lint/alloclint/LOWER-A-ROW.md)")
    for key in sorted(led):
        if key[1] == "ir-only" and not a.ir_misses:
            continue
        if led[key] > cur.get(key, 0):
            f, k, it = key
            fails.append(f"alloclint FAIL STALE {f} {k} {it}: budget {led[key]}, now {cur.get(key, 0)}: lower the row "
                         f"(by hand, or crates/gravity/lint/alloclint/ratchet.py write --crate {findings_mod.crate_dir(f, crates) or '?'}; "
                         f"crates/gravity/lint/alloclint/LOWER-A-ROW.md)")
    # the S1 leg (alloclintcheck.sh exports ALLOCLINT_BASE = the range's merge-base): each renames.tsv row the range adds, for a
    # checked crate, is printed and its new item's finding must sit on its line in the tree (the inventory's site key)
    rb = os.environ.get("ALLOCLINT_BASE", "")
    if rb:
        for r in sorted(renames_at(a.root, None, True) - renames_at(a.root, rb)):
            f, ln = r[0], r[1]
            if not findings_mod.crate_dir(f, crates):
                continue
            if r[4] == "ir-only" and not a.ir_misses:
                # (planmem 2026-10-15T15:46:58Z, main 15:47:11Z GO) as the STALE loop: no IR leg here (S1 runs clippy only), so an
                # ir-only row cannot name its finding; 'ratchet.py base' and S3-slow's IR leg judge it where IR exists
                print(f"alloclint RENAME {f}:{ln} {r[2]} {r[3]} -> {r[4]} {r[5]} x{r[6]} ({r[7]}: {r[8]}): ir-only, not judged without the IR leg")
                continue
            at = lines.get((f, r[4], r[5]), [])
            if ln.isdigit() and int(ln) in at:
                print(f"alloclint RENAME {f}:{ln} {r[2]} {r[3]} -> {r[4]} {r[5]} x{r[6]} ({r[7]}: {r[8]})")
            else:
                fails.append(f"alloclint FAIL RENAME {f}:{ln} {r[4]} {r[5]}: no such finding on that line in the tree (its lines: "
                             f"{','.join(map(str, sorted(at))) or 'none'}): the row names no site")
    for x in fails:
        print(x)
    # (MISTAGGED is a count only, in the tail line and .columns: no line per site (main 2026-10-14T19:04:45Z: the ~1,650 such
    # lines are gravity_lint's Cold LIFETIME tags on cold paths, debt before and after)
    total = sum(v for k, v in cur.items())
    if fails:
        print(f"alloclint: FAIL ({len(fails)} line(s)); {total} sites now (debt), {sum(led.values())} budgeted in {len(keys)} crate(s); "
              f"{cold_say(cold)}")
        return 1
    print(f"alloclint: PASS: {total} untracked allocation sites (debt) in {len({k[0] for k in cur})} files of {len(keys)} crate(s), "
          f"each at its budget; {cold_say(cold)}")
    return 0


def comment_lines(p):
    """(path line, '#' lines) of a ledger file: its notes verbatim and in order (a user-approved add's note: `write` keeps
    them, memacct 10-15)"""
    try:
        ls = open(p).read().splitlines()
    except OSError:
        return "", []
    return next((ln for ln in ls if ln.startswith("path\t")), ""), [ln for ln in ls if ln.startswith("#")]


def cmd_write(a):
    crates = crates_of(a)
    fresh = parse_noalloc(open(a.noalloc).read()) if getattr(a, "noalloc", None) else None
    cur, _, n, cold = current(a.findings, a.ir_misses, crates, a.root, fresh)
    # --file (repeatable, a source path): the write touches ONLY those files' ledger files (memacct 10-15: a lane rewrote
    # rows of a file outside its change); every other file of the crate, its .columns and .empty stay as they are
    only = {f.rstrip("/") for f in (getattr(a, "file", None) or [])}
    full = not a.crate and not only   # only a full write (every crate) records the .columns files: a lane's --crate write keeps them
    if n == 0:
        print("alloclint: refusing to write an empty ledger (no finding)")
        return 2
    d = os.path.join(a.root, LEDGER)
    os.makedirs(d, exist_ok=True)
    total = files = 0
    # a crate with no ledger directory is being SEEDED: an IR-only seed means the clippy leg found nothing for it (an empty
    # replay: cargo found the crate fresh from a plain dependency check), and the seed would read budget 0 for every clippy row
    # (parquet_read, alloclint#3286, main 2026-10-14T21:51:02Z). collect.sh re-lints such a crate; this refuses what slips by.
    for c in crates:
        if os.path.isdir(os.path.join(d, crate_key(c))):
            continue
        mine = collections.Counter()
        for (f, k, _), v in cur.items():
            if findings_mod.crate_dir(f, crates) == c:
                mine[k] += v
        if sum(mine.values()) >= SEED_IR_ONLY_MIN and set(mine) == {"ir-only"}:   # base's FAIL SEED rule, the same threshold
            print(f"alloclint: refusing to seed {c}: {mine['ir-only']} IR-only site(s) and no clippy finding (an empty clippy replay; "
                  f"collect.sh re-lints a crate with no ledger: rerun run.sh clippy / full for it, then write)")
            return 2
    for c in crates:
        cd = os.path.join(d, crate_key(c))
        keep = {}
        if not a.ir_misses:   # a clippy-only write keeps the IR leg's rows as they are
            old = read_ledger_dir(d, {crate_key(c)}) or {}
            keep = {k: v for k, v in old.items() if k[1] == "ir-only"}
        mine_names = {ledger_name(f) for f in only if findings_mod.crate_dir(f, crates) == c}
        if only and not mine_names:
            continue
        # --lower-only (memacct 2026-10-15T21:0xZ: a new debt row needs the user's word): every row is min(budget, now); a row
        # the ledger does not hold is not added, a budget never rises. The NEW rows stay NEW (booked, owned, converted).
        floor = (read_ledger_dir(d, {crate_key(c)}) or {}) if getattr(a, "lower_only", False) else None
        notes = {}
        if os.path.isdir(cd):
            for name in os.listdir(cd):
                if only and name not in mine_names:
                    continue
                if not name.startswith("."):
                    notes[name] = comment_lines(os.path.join(cd, name))
                if name == NOALLOC_FILE:
                    continue   # rewritten below
                if name != ".columns" or full:
                    os.remove(os.path.join(cd, name))
        os.makedirs(cd, exist_ok=True)
        by_file = collections.defaultdict(list)
        for (f, k, it), v in list(cur.items()) + list(keep.items()):
            if findings_mod.crate_dir(f, crates) == c and (not only or f in only):
                if floor is not None:
                    v = min(v, floor.get((f, k, it), 0))
                    if v <= 0:
                        continue
                by_file[f].append((k, it, v))
        for f, rows in by_file.items():
            with open(os.path.join(cd, ledger_name(f)), "w") as o:
                o.write(f"path\t{f}\n")
                for ln in notes.pop(ledger_name(f), ("", []))[1]:
                    o.write(ln + "\n")
                for k, it, v in sorted(rows):
                    o.write(f"{k}\t{it}\t{v}\n")
            total += sum(v for _, _, v in rows)
        for name, (pl, ls) in notes.items():
            if ls:   # a file whose rows all went keeps its notes (no row: budget 0), so no approved note is lost
                with open(os.path.join(cd, name), "w") as o:
                    o.write("\n".join(([pl] if pl else []) + ls) + "\n")
        files += len(by_file)
        # .noalloc: the IR leg's rows for this crate (a write with --noalloc), else the rows kept whose line still reads the same
        # (a clippy-only write never adds one; an edited line's row goes, its finding is debt until a full run)
        nap = os.path.join(cd, NOALLOC_FILE)
        rows_na = {k: v for k, v in (fresh if fresh is not None else read_noalloc(a.root, [c])).items()
                   if findings_mod.crate_dir(k[0], crates) == c and (not only or k[0] in only or fresh is None)}
        if only and fresh is not None:
            rows_na.update({k: v for k, v in read_noalloc(a.root, [c]).items() if k[0] not in only})
        keep_na, keys_in, taken = {}, {}, collections.defaultdict(set)
        # rows in line order, so rows of the same text keep their order; each takes the nearest line of its text NOT taken by an
        # earlier row of the same (file, kind, item): two rows of a repeated line text (`let x = y.clone();` at 8 lines) used to
        # land on one nearest line after the file moved and overwrite each other, the dropped rows' findings then read as debt
        # (FAIL NEW at the next check: alloclint 2026-10-16, a --lower-only write on trunk af977bcdaac lost 7 of pgrc_read /
        # exec / hosts's NOALLOC rows)
        for k, (cnt, tk, cls) in sorted(rows_na.items()):
            if k[0] not in keys_in:
                full_p = os.path.join(a.root, k[0])
                ls = open(full_p, errors="replace").read().split("\n") if os.path.exists(full_p) else []
                keys_in[k[0]] = collections.defaultdict(list)
                for i, t_ in enumerate(ls, 1):
                    keys_in[k[0]][findings_mod.line_key(t_)].append(i)
            at = [i for i in keys_in[k[0]].get(tk, ()) if i not in taken[(k[0], k[2], k[3])]]
            if at:   # the line's text is still in the file: the row follows it (the nearest such line no other row took)
                ln = min(at, key=lambda i: abs(i - k[1]))
                taken[(k[0], k[2], k[3])].add(ln)
                keep_na[(k[0], ln, k[2], k[3])] = (cnt, tk, cls)
        if keep_na:
            with open(nap, "w") as o:
                o.write("# NOALLOC (TD-197 LINT-FP; findings.NOALLOC_ITEMS): findings the IR leg proved allocate nothing, keyed on the "
                        "line's normalized text. Written by run.sh full + ratchet.py write --noalloc only; an edited line's row goes.\n"
                        "file\tline\tkind\titem\tcount\ttext\tclass\n")
                for k, (cnt, tk, cls) in sorted(keep_na.items()):
                    o.write(f"{k[0]}\t{k[1]}\t{k[2]}\t{k[3]}\t{cnt}\t{tk}\t{cls}\n")
        elif os.path.exists(nap):
            os.remove(nap)
        if not by_file and not only and not any(not x.startswith(".") for x in os.listdir(cd)):
            open(os.path.join(cd, ".empty"), "w").write("")   # a crate with no site keeps its directory (judged, at zero)
        if full:
            mine = [x for x in cold if findings_mod.crate_dir(x[0], crates) == c]
            t, u, m = cold_columns(mine)
            debt = sum(v for (f, _, _), v in cur.items() if findings_mod.crate_dir(f, crates) == c)
            open(os.path.join(cd, ".columns"), "w").write(
                "# the crate's columns at the last full `ratchet.py write` (never judged; USER RULING 2026-10-14T18:52:43Z: COLD-TEXT\n"
                "# = Cold-tagged error / refusal / log text, a named class out of the debt total; COLD-UNPROVEN and MISTAGGED are in it;\n"
                "# USER RULING via main 2026-10-14T23:20:10Z: TYPE-MENTION = a type named on a line with no allocation finding, out of it)\n"
                "# USER GO via main 2026-10-15T11:46:31Z: REFCOUNT-CLONE = an Rc / Arc / Weak clone alone on its line, out of it too\n"
                "# USER RULING via memacct 2026-10-15T18:5xZ: REFCOUNT-CLONE-INLINE = one beside other findings, out of it too\n"
                "# TD-197 amendment (memacct GO 2026-10-15T20:3xZ): NOALLOC = a Clone / ToOwned / Extend the IR proved allocates\n"
                "# nothing (.noalloc), UNREACHED = a finding in a function only test code reaches (unreached.tsv), out of it too\n"
                f"debt\t{debt}\ntype-mention\t{mentions(mine)}\ncold-text\t{t}\ncold-unproven\t{u}\nmistagged\t{m}\n"
                f"refcount-clone\t{refcounts(mine)}\nrefcount-clone-inline\t{refcounts_inline(mine)}\n"
                f"noalloc\t{named_count(mine, 'NOALLOC')}\nunreached\t{named_count(mine, 'UNREACHED')}\n")
    print(f"alloclint: wrote {total} sites (debt) in {files} files of {len(crates)} crate(s) to {LEDGER}; {cold_say(cold)}")
    return 0


def git(repo, *args):
    r = subprocess.run(["git", "-C", repo] + list(args), capture_output=True, text=True)
    return r.returncode, r.stdout


APPROVED = "Alloclint-Approved: "


def approved_fields(line):
    """(file, kind, item, count, citation) of an `Alloclint-Approved: <file> <kind> <item> <count> <citation>` trailer line,
    or None when it is malformed. The item may hold spaces (an IR item: `<T as core::convert::Into<U>>::into`), so the words
    are taken from both ends: file = 1st, kind = 2nd, citation = last, count = second-to-last (digits), item = every word
    between, joined by single spaces (the ledger's item, a tab-separated column, keeps its spaces the same way). A 6-word
    trailer parses as it always did; fewer than 5 words after the key is malformed. No quoting."""
    if not line.startswith(APPROVED):
        return None
    w = line[len(APPROVED):].split()
    if len(w) < 5 or not w[-2].isdigit():
        return None
    return w[0], w[1], " ".join(w[2:-2]), int(w[-2]), w[-1]


def ledger_at(repo, rev):
    """The ledger rows at `rev` (None when `rev` has no ledger), read in one `git cat-file --batch` (a `git show` per file
    is ~6 s for the 381 files)."""
    rc, out = git(repo, "ls-tree", "-r", "--name-only", f"{rev}:{LEDGER}")
    if rc != 0:
        return None
    names = [n for n in out.split() if n != "README" and not n.rsplit("/", 1)[-1].startswith(".")]
    r = subprocess.run(["git", "-C", repo, "cat-file", "--batch"], input="".join(f"{rev}:{LEDGER}/{n}\n" for n in names).encode(),
                       capture_output=True, check=True)
    buf, i, rows = r.stdout, 0, collections.Counter()
    for n in names:
        head_end = buf.index(b"\n", i)
        size = int(buf[i:head_end].split()[2])
        rows.update(parse_rows(n.rsplit("/", 1)[-1], buf[head_end + 1:head_end + 1 + size].decode()))
        i = head_end + 1 + size + 1
    return rows


SCOPE = "crates/gravity/lint/alloclint/scope.tsv"


def scope_at(repo, rev, tree_is_worktree=False):
    """{crate dir: None (the whole crate) | {its in-scope files, relative to the dir}} of scope.tsv at rev"""
    if tree_is_worktree:
        p = os.path.join(repo, SCOPE)
        text = open(p).read() if os.path.exists(p) else ""
    else:
        rc_, text = git(repo, "show", f"{rev}:{SCOPE}")
        text = text if rc_ == 0 else ""
    out = {}
    for ln in text.splitlines():
        if not ln.strip() or ln.startswith("#"):
            continue
        c = ln.split("\t")
        out[c[0].rstrip("/")] = set(x.strip() for x in c[3].split(",") if x.strip()) if len(c) > 3 and c[3].strip() else None
    return out


def in_scope(sc, f):
    """the crate dir of file f in a scope map (the longest listed prefix), when f is in scope there; else None"""
    best = None
    for d, files in sc.items():
        if f.startswith(d + "/") and (best is None or len(d) > len(best)):
            best = d
    if best is None:
        return None
    files = sc[best]
    return best if files is None or f[len(best) + 1:] in files else None


def renames_at(repo, rev, worktree=False):
    """the rename rows (file, line, old_kind, old_item, new_kind, new_item, count, lane, reason) of renames.tsv at REV"""
    if worktree:
        p = os.path.join(repo, RENAMES)
        text = open(p).read() if os.path.exists(p) else ""
    else:
        rc, text = git(repo, "show", f"{rev}:{RENAMES}")
        text = text if rc == 0 else ""
    rows = set()
    for ln in text.splitlines():
        c = ln.rstrip("\n").split("\t")
        if ln.startswith("#") or not ln.strip() or c[0] == "file" or len(c) < 9:
            continue
        rows.add(tuple(c[:9]))
    return rows


def crate_renames_at(repo, rev, worktree=False):
    """[(new crate dir, old crate dir)] of the tree's CRATE_RENAMES, longest new dir first (empty when it has none)"""
    if worktree:
        p = os.path.join(repo, CRATE_RENAMES)
        text = open(p).read() if os.path.exists(p) else ""
    else:
        rc, text = git(repo, "show", f"{rev}:{CRATE_RENAMES}")
        text = text if rc == 0 else ""
    out = []
    for ln in text.splitlines():
        c = ln.split("\t")
        if ln.strip() and not ln.lstrip().startswith("#") and len(c) >= 2 and c[0].strip() and c[1].strip():
            out.append((c[0].strip().rstrip("/"), c[1].strip().rstrip("/")))
    return sorted(out, key=lambda x: -len(x[0]))


def crate_moved(cmap, f):
    """f's path under its old crate dir when f is under a mapped new dir (the longest), else None"""
    for new, was in cmap:
        if f.startswith(new + "/"):
            return was + f[len(new):]
    return None


def item_at_base(item, cmap, f, src):
    """the item's name before the range's renames: each mapped crate's name (its dir's last segment) spelled as the old
    crate's (pgrc_format::try_vec -> pgrc_format::try_vec), and a moved file's module stem as the old file's
    (copy_cmd::parquet_pgrc::DecodeShared -> copy_cmd::parquet_pgrc::DecodeShared)"""
    import re
    pairs = [(n.rsplit("/", 1)[-1], o.rsplit("/", 1)[-1]) for n, o in cmap]
    if src != f:
        ns, os_ = os.path.splitext(os.path.basename(f))[0], os.path.splitext(os.path.basename(src))[0]
        if ns not in ("lib", "mod", "main") and os_ not in ("lib", "mod", "main"):
            pairs.append((ns, os_))
    for n, o in pairs:
        if n != o:
            item = re.sub(r"(?<![A-Za-z0-9_])" + re.escape(n) + r"::", o + "::", item)
    return item


def item_token(item):
    """the name a source line spells for a detector item: its last path segment (std::clone::Clone::clone -> clone,
    slice::to_vec -> to_vec, std::vec::Vec -> Vec)"""
    return item.split("::")[-1].split("<")[0].strip()


def same_site(repo, base, tree_rev, f, line, old_item, new_item, src=None):
    """(base line, None) when the tree's line `line` of f is a CHANGED line whose paired base line (git diff -U0: in a change
    block the k-th '+' line pairs with the k-th '-' line) names the old item and which names the new one; else (None, why).
    src = f's path at the base when the range moved it (git's pair or the crate map): the blobs are diffed across the move."""
    if src and src != f:
        rc, out = git(repo, "diff", "-U0", "--no-color", f"{base}:{src}", f"{tree_rev}:{f}")
    else:
        rc, out = git(repo, "diff", "-U0", "--no-color", base, tree_rev, "--", f)
    if rc != 0:
        return None, "git diff failed"
    minus, plus, a0, c0 = [], [], 0, 0
    blocks = []
    for ln in out.splitlines():
        if ln.startswith("@@"):
            if minus or plus:
                blocks.append((a0, minus, c0, plus))
            hdr = ln.split("@@")[1].split()
            a0 = int(hdr[0][1:].split(",")[0])
            c0 = int(hdr[1][1:].split(",")[0])
            minus, plus = [], []
        elif ln.startswith("-") and not ln.startswith("---"):
            minus.append(ln[1:])
        elif ln.startswith("+") and not ln.startswith("+++"):
            plus.append(ln[1:])
    if minus or plus:
        blocks.append((a0, minus, c0, plus))
    ot, nt = item_token(old_item), item_token(new_item)
    norm = lambda x: " ".join(x.split())
    for a0, minus, c0, plus in blocks:
        if c0 <= line < c0 + len(plus):
            k = line - c0
            if nt not in plus[k]:
                return None, f"line {line} does not name `{nt}`"
            # the same site: a removed line of the same change block that is this line with the new item's name put back to the
            # old one (only the item changed), else the line at the same place in a block that keeps its line count
            for j, m in enumerate(minus):
                if ot in m and norm(m.replace(ot, nt)) == norm(plus[k]):
                    return a0 + j, None
            if len(minus) == len(plus) and ot in minus[k]:
                return a0 + k, None
            return None, (f"no removed line of its change block (base {a0}..{a0 + len(minus) - 1}) is this line with `{nt}` spelled "
                          f"`{ot}`: a new site or a moved one")
    return None, f"line {line} is not a changed line in the range"


def noalloc_at(repo, rev, worktree=False):
    """{crate key: {(file, line, kind, item): (count, text)}} of the ledger's .noalloc files at REV (worktree: on disk)"""
    out = {}
    if worktree:
        d = os.path.join(repo, LEDGER)
        for k in (os.listdir(d) if os.path.isdir(d) else []):
            p = os.path.join(d, k, NOALLOC_FILE)
            if os.path.exists(p):
                out[k] = parse_noalloc(open(p).read())
        return out
    rc, names = git(repo, "ls-tree", "-r", "--name-only", f"{rev}:{LEDGER}")
    for n in (names.split() if rc == 0 else []):
        if n.endswith("/" + NOALLOC_FILE):
            rc2, t = git(repo, "show", f"{rev}:{LEDGER}/{n}")
            out[n.split("/")[0]] = parse_noalloc(t if rc2 == 0 else "")
    return out


def mcx_dir_at(repo, rev):
    """the mcx-transmute ledger directory exists at REV (None: the worktree)"""
    if rev is None:
        return os.path.isdir(os.path.join(repo, LEDGER, mcxt.SUBDIR))
    return git(repo, "rev-parse", "-q", "--verify", f"{rev}:{LEDGER}/{mcxt.SUBDIR}")[0] == 0


def cmd_base(a):
    tree = a.tree
    if tree is None:
        new = read_ledger_dir(os.path.join(a.repo, LEDGER))
        tree_rev = "HEAD"
    else:
        new = ledger_at(a.repo, tree)
        tree_rev = tree
    for rev in (a.base, tree_rev):
        # a tree-ish: check.sh's staged mode (the pre-commit hook) passes `git write-tree`'s tree, not a commit
        if git(a.repo, "rev-parse", "--verify", "-q", rev + "^{tree}")[0] != 0:
            print(f"alloclint base: CANNOT JUDGE: no commit or tree {rev}")
            return 2
    log_rev = tree_rev if git(a.repo, "rev-parse", "--verify", "-q", tree_rev + "^{commit}")[0] == 0 else "HEAD"
    old = ledger_at(a.repo, a.base)
    if old is None:
        print("alloclint base: SKIP: the base has no ledger (before the seed)")
        return 0
    if new is None:
        print(f"alloclint base FAIL: the tree deletes {LEDGER}: lower its rows instead (the check needs the ledger)")
        return 1
    renames = {}
    rc, out = git(a.repo, "diff", "-M", "--name-status", a.base, tree_rev, "--", "crates")
    for ln in out.splitlines():
        c = ln.split("\t")
        if c[0].startswith("R") and len(c) == 3:
            renames[c[2]] = c[1]
    # CRATE RENAMES: a file git does not pair keeps its old path's rows through the tree's crate map (src_of: git's pair first)
    cmap = crate_renames_at(a.repo, tree_rev, tree is None)
    cmoved = {}
    for f in sorted({k[0] for k in new}):
        o = crate_moved(cmap, f)   # consulted when git's pair (if any) has no row for the key, as alloc_ratchet.py does
        if o is not None:
            cmoved[f] = o

    def src_of(f):
        return renames.get(f) or cmoved.get(f) or f
    carried = collections.Counter()   # new crate dir -> rows carried at or under their old budget
    # a SCOPE EXTENSION seeds a crate's (or a listed file's) rows: a file in scope at the tree but not at the base may ADD rows
    # when a commit in the range names its crate dir in a trailer `Alloclint-Scope: <crate dir>` (the reviewer's record, as a
    # home's); the seed is the crate's debt as it stands, and from then on its rows only go down
    sc_new, sc_old = scope_at(a.repo, tree_rev, tree is None), scope_at(a.repo, a.base)
    _, msgs_ = git(a.repo, "log", "--format=%B", f"{a.base}..{log_rev}")
    scope_named = {m.split()[1].rstrip("/") for m in msgs_.splitlines() if m.startswith("Alloclint-Scope: ") and len(m.split()) >= 2}
    # a RENAME (main 2026-10-14T21:10:42Z ruling on planir's ask): the same site's allocation changes detector item (an IR list
    # no longer Clone: b.params.clone() -> b.params.to_vec()). A row added to renames.tsv in the range lets the new item rise by
    # its count at that site when (1) the site is the same: the tree's line is a changed line paired with a base line naming the
    # old item (git diff -U0); (2) the file's total debt does not rise; (3) the row is in renames.tsv (the S1 leg prints it and
    # checks the new item's finding sits on that line); (4) the old item falls by at least the count. Never a new site.
    ren_rows = renames_at(a.repo, tree_rev, tree is None) - renames_at(a.repo, a.base)
    allow, need, ren_files, fails = collections.Counter(), collections.Counter(), set(), []
    for r in sorted(ren_rows):
        f, ln, ok, oi, nk, ni, cnt, lane, why = r
        try:
            ln, cnt = int(ln), int(cnt)
        except ValueError:
            fails.append(f"alloclint base FAIL RENAME {f}: line / count not a number: {r}")
            continue
        bl, err = same_site(a.repo, a.base, tree_rev, f, ln, oi, ni, src_of(f))
        if err:
            fails.append(f"alloclint base FAIL RENAME {f}:{ln} {ok} {oi} -> {nk} {ni}: cannot prove the same site ({err}); "
                         f"route the allocation through a home, or post FOR MAIN")
            continue
        allow[(f, nk, ni)] += cnt
        need[(f, ok, oi)] += cnt
        ren_files.add(f)
        print(f"alloclint RENAME {f}:{ln} (base line {bl}) {ok} {oi} -> {nk} {ni} x{cnt} ({lane}: {why})")
    # a USER-APPROVED ADD (the user's option A on planprod's P6-c2 rows, via main 2026-10-15T14:56:41Z): a row the user accepted
    # as new debt is named in a commit trailer of the range, `Alloclint-Approved: <file> <kind> <item> <count> <decision>` (the
    # item may hold spaces: approved_fields reads the words from both ends), the
    # decision a `USER-DECISIONS-*.md@<ts>` citation; the row may be added or raised by at most <count>. No citation, no allowance
    # (a malformed trailer FAILs). Each approval is printed; from then on the row only goes down.
    approved = 0
    for m in msgs_.splitlines():
        if not m.startswith(APPROVED):
            continue
        ap = approved_fields(m.strip())
        if ap is None or not ap[4].startswith("USER-DECISIONS-"):
            fails.append(f"alloclint base FAIL APPROVED: malformed trailer {m.strip()!r} (want `Alloclint-Approved: <file> <kind> <item> "
                         f"<count> USER-DECISIONS-<file>.md@<ts>`, the user's decision cited; the item may hold spaces)")
            continue
        af, ak, ai, an, ac = ap
        allow[(af, ak, ai)] += an
        approved += an
        print(f"alloclint APPROVED {af} {ak} {ai}: up to +{an} ({ac})")
    # the TYPE-MENTION CATCH-UP (main 2026-10-14T23:41:27Z ruling 1, TD-197 amendment 3): the range that brings TYPE-MENTION in
    # (its base's findings.py has no is_mention) and names it in a trailer `Alloclint-Catchup: type-mention` may raise a file's
    # ir-only rows: an IR allocation that a type row stood for (crosscheck covered it by the type finding) is now its own
    # ir-only row, as a mention covers nothing. Per file: the raise is accepted only when the file's type + ir-only total does
    # not rise against the base. Each accepted row is printed (S0 / S1 show it). One range only: a later base has is_mention.
    _, base_findings = git(a.repo, "show", f"{a.base}:crates/gravity/lint/alloclint/findings.py")
    catchup = (any(m.strip() == "Alloclint-Catchup: type-mention" for m in msgs_.splitlines())
               and "def is_mention" not in base_findings)
    ti_old, ti_new = collections.Counter(), collections.Counter()
    for (f, k, _), v in old.items():
        if k in ("type", "ir-only"):
            ti_old[f] += v
    for (f, k, _), v in new.items():
        if k in ("type", "ir-only"):
            ti_new[src_of(f)] += v
    caught, bad_files = 0, set()

    def catch(key, was, v):
        nonlocal caught
        src = src_of(key[0])
        if not (catchup and key[1] == "ir-only"):
            return False
        if ti_new[src] > ti_old[src]:
            if src in bad_files:
                return True
            bad_files.add(src)
            fails.append(f"alloclint base FAIL CATCH-UP {key[0]}: its type + ir-only total rises {ti_old[src]} -> {ti_new[src]} "
                         f"(the TYPE-MENTION catch-up only moves a type row's allocation to an ir-only row; post FOR MAIN)")
            return True
        print(f"alloclint CATCH-UP {key[0]} ir-only {key[2]}: {was} -> {v} (TYPE-MENTION, TD-197 amendment 3: the file's type + "
              f"ir-only {ti_old[src]} -> {ti_new[src]})")
        caught += v - was
        return True
    # the CHARGED-CALLER SEED (D4505, USER GO via main 2026-10-15T05:52:10Z; homes.caller_lines): a `charged caller` tag outside a
    # pass-through writer the alloclint clippy.toml lists is no home from now on, so its line's findings are MISTAGGED debt. The
    # range that brings the rule in (its base's homes.py has no caller_lines) and names it in a trailer
    # `Alloclint-Seed: charged-caller` may add / raise the rows of a file that holds unlisted `charged caller` lines in the tree
    # and whose source the range does not change (so the rise is the rule's, the sites the tag hid entering the ledger once), by
    # at most CC_PER_LINE findings per such line (a line's call, plus the type or copy beside it: sidecar.rs:372
    # `Vec::with_capacity` + `Vec`, :395 `push` + `to_vec`). The S1 / S2 clippy leg is exact (rows equal the findings), so the
    # bound only keeps the seed to these files. Each accepted row is printed; any other rise FAILs as before. One range only: a
    # later base has the rule.
    _, base_homes = git(a.repo, "show", f"{a.base}:crates/gravity/lint/alloclint/homes.py")
    cc_seed = (any(m.strip() == "Alloclint-Seed: charged-caller" for m in msgs_.splitlines())
               and "def caller_lines" not in base_homes)
    cc_room, cc_listed, cc_seeded, cc_files = {}, [], 0, set()

    def tree_text(p):
        if tree is None:
            full = os.path.join(a.repo, p)
            return open(full, errors="replace").read() if os.path.exists(full) else ""
        rc_, t = git(a.repo, "show", f"{tree_rev}:{p}")
        return t if rc_ == 0 else ""

    def cc_take(key, was, v):
        nonlocal cc_seeded
        if not cc_seed:
            return False
        if not cc_listed:
            cc_listed.append(homes_mod.listed_writers(tree_text(homes_mod.CLIPPY)))
        f = key[0]
        if f not in cc_room:
            same = (git(a.repo, "diff", "--quiet", a.base, "--", f) if tree is None
                    else git(a.repo, "diff", "--quiet", a.base, tree_rev, "--", f))[0] == 0
            cc_room[f] = CC_PER_LINE * len(homes_mod.caller_lines(f, tree_text(f), cc_listed[0])) if same else 0
        if cc_room[f] < v - was:
            return False
        cc_room[f] -= v - was
        cc_seeded += v - was
        cc_files.add(f)
        print(f"alloclint SEED charged-caller {f} {key[1]} {key[2]}: {was} -> {v} (D4505: a `charged caller` tag outside a listed "
              f"pass-through writer is MISTAGGED debt; room left in the file {cc_room[f]})")
        return True
    seeded, seeded_dirs, unnamed = 0, set(), set()
    seeded_kinds = collections.defaultdict(collections.Counter)   # crate dir -> kind -> sites seeded
    # MCX-TRANSMUTE (USER APPROVED via main 2026-10-15T00:10:23Z; mcxt.py): the range that brings the kind's ledger in seeds
    # its rows when a commit names it in a trailer `Alloclint-Seed: mcx-transmute` (the census as new debt rows, this ruling
    # only); after that its rows follow the law (never added, never raised) and every changed file's sites EQUAL its rows
    mcx_on_tree, mcx_on_base = mcx_dir_at(a.repo, None if tree is None else tree_rev), mcx_dir_at(a.repo, a.base)
    mcx_seeding = mcx_on_tree and not mcx_on_base
    mcx_seed_named = any(m.strip() == "Alloclint-Seed: " + mcxt.KIND for m in msgs_.splitlines())
    mcx_seeded = 0
    for key, v in sorted(new.items()):
        if mcx_seeding and key[1] == mcxt.KIND and mcx_seed_named and old.get(key) is None:
            mcx_seeded += v
            continue
        was = old.get(key)
        if was is None and key[0] in renames:
            was = old.get((renames[key[0]], key[1], key[2]))
        if was is None and cmap:
            # the row moved with its crate (its path, and the renamed crates' names in its item); the old row still in the
            # tree = a new row
            src = src_of(key[0])
            for ok_ in ((src, key[1], key[2]), (src, key[1], item_at_base(key[2], cmap, key[0], src))):
                if ok_ != key and ok_ in old and ok_ not in new:
                    was = old[ok_]
                    carried[next((n for n, _ in cmap if key[0].startswith(n + "/")), "(items)")] += 1
                    break
        if was is None:
            d = in_scope(sc_new, key[0])
            if d is not None and in_scope(sc_old, key[0]) is None and in_scope(sc_old, src_of(key[0])) is None:
                if d in scope_named:
                    seeded += v
                    seeded_dirs.add(d)
                    seeded_kinds[d][key[1]] += v
                    continue
                unnamed.add(d)
            if allow[key] >= v:
                allow[key] -= v
                continue
            if catch(key, 0, v):
                continue
            if cc_take(key, 0, v):
                continue
            fails.append(f"alloclint base FAIL ADDS {key[0]} {key[1]} {key[2]} = {v}: a new untracked allocation site is not "
                         f"accepted into the ledger; route it through a home")
        elif v > was:
            if allow[key] >= v - was:
                allow[key] -= v - was
                continue
            if catch(key, was, v):
                continue
            if cc_take(key, was, v):
                continue
            fails.append(f"alloclint base FAIL RAISES {key[0]} {key[1]} {key[2]}: {was} -> {v}")
    for key, n in sorted(need.items()):
        src = src_of(key[0])
        fell = old.get((src, key[1], key[2]), 0) - new.get(key, 0)
        if fell < n:
            fails.append(f"alloclint base FAIL RENAME {key[0]} {key[1]} {key[2]}: the old item falls by {fell}, the rename(s) need {n} "
                         f"(an item rises only by what another falls at the same site)")
    for f in sorted(ren_files):
        src = src_of(f)
        t_new = sum(v for k, v in new.items() if k[0] == f)
        t_old = sum(v for k, v in old.items() if k[0] == src)
        if t_new > t_old:
            fails.append(f"alloclint base FAIL RENAME {f}: the file's debt rises {t_old} -> {t_new} (a rename never adds debt)")
    # a new home is a reviewed exemption: each added homes.tsv row needs an `Alloclint-Home: <file> <function>` trailer in the range
    def home_rows(rev):
        if rev == "HEAD" and tree is None:
            p = os.path.join(a.repo, HOMES)
            text = open(p).read() if os.path.exists(p) else ""
        else:
            rc_, text = git(a.repo, "show", f"{rev}:{HOMES}")
            text = text if rc_ == 0 else ""
        return {tuple(ln.split("\t")[:2]) for ln in text.splitlines() if ln.strip() and not ln.startswith("#")}
    h_tree, h_base = home_rows(tree_rev), home_rows(a.base)
    # NOALLOC rows are the IR leg's: a .noalloc row the range adds or raises needs a trailer `Alloclint-Noalloc: <crate dir>`
    # (the record that run.sh full + ratchet.py write --noalloc derived it); the S3-slow alloclint-ir slot re-derives every row
    na_new, na_old = noalloc_at(a.repo, tree_rev, tree is None), noalloc_at(a.repo, a.base)
    na_named = {m.split()[1].rstrip("/") for m in msgs_.splitlines() if m.startswith("Alloclint-Noalloc: ") and len(m.split()) >= 2}
    na_added = 0
    for ck, rows_ in sorted(na_new.items()):
        old_t = noalloc_by_text(na_old.get(ck, {}))
        new_t = noalloc_by_text(rows_)
        up = [((key[0], "?", key[1], key[2]), cnt) for key, cnt in sorted(new_t.items()) if cnt > old_t.get(key, 0)]
        if not up:
            continue
        cdir = next((d for d in sc_new if crate_key(d) == ck), ck)
        if cdir in na_named or ck in na_named:
            na_added += len(up)
            continue
        fails.append(f"alloclint base FAIL NOALLOC {cdir}: {len(up)} .noalloc row(s) added or changed ({up[0][0][0]}:{up[0][0][1]} "
                     f"{up[0][0][3]} ..): only a full run writes them; name the crate in a trailer `Alloclint-Noalloc: {cdir}`")
    # UNREACHED: every unreached.tsv row is proven again on the tree (unreached.py: the name nowhere outside test code)
    tr_u = unreached_mod.Tree(a.repo, None if tree is None else tree_rev)
    u_rows = unreached_mod.read_rows(tr_u.text(unreached_mod.LIST))
    if u_rows:
        u_res = unreached_mod.prove(tr_u, [(f_, fn_) for f_, fn_, _ in u_rows])
        for f_, fn_, note_ in u_rows:
            if fn_ == "*" and unreached_mod.FILE_PROOF not in note_:
                u_res[(f_, fn_)] = [f"the FILE row's note does not name {unreached_mod.FILE_PROOF!r} and its exclusions"] + u_res[(f_, fn_)]
        for (f_, fn_), hs in sorted(u_res.items()):
            if hs:
                fails.append(f"alloclint base FAIL UNREACHED {f_} {fn_}: reached ({len(hs)} hit(s): {' | '.join(hs[:2])}): drop the row; "
                             f"its sites are debt rows again (route them through a home)")
    added = h_tree - h_base
    # a home whose file moved (git's pair, or the crate map) is the same home when the tree drops its old row
    moved_homes = {(f, fn) for f, fn in added
                   if (renames.get(f) or crate_moved(cmap, f) or f) != f
                   and ((renames.get(f) or crate_moved(cmap, f)), fn) in h_base - h_tree}
    for f, fn in sorted(moved_homes):
        print(f"alloclint HOME MOVED {f} {fn} <- {renames.get(f) or crate_moved(cmap, f)} (the same home, its file renamed)")
    added -= moved_homes
    if added:
        _, msgs = git(a.repo, "log", "--format=%B", f"{a.base}..{log_rev}")
        named = {tuple(m.split()[1:3]) for m in msgs.splitlines() if m.startswith("Alloclint-Home: ") and len(m.split()) >= 3}
        for f, fn in sorted(added):
            if (f, fn) not in named:
                fails.append(f"alloclint base FAIL HOME {f} {fn}: a new home exempts its sites; name it in a commit trailer "
                             f"`Alloclint-Home: {f} {fn}` (the reviewer's record) or route the allocation through an existing home")
    # a seed of IR-only rows is an empty clippy replay, not the crate's debt: its clippy rows would read budget 0 at the leg
    # (parquet_read, alloclint#3286). A crate's real seed names its types / methods / growth / macros.
    for d in sorted(seeded_dirs):
        k = seeded_kinds[d]
        if sum(k.values()) >= SEED_IR_ONLY_MIN and set(k) == {"ir-only"}:
            fails.append(f"alloclint base FAIL SEED {d}: its {k['ir-only']} seeded site(s) are all ir-only, no clippy-kind row "
                         f"(the clippy leg found nothing for it: an empty replay). Re-cut: run.sh full + ratchet.py write --crate {d}")
    for d in sorted(unnamed):
        fails.append(f"alloclint base FAIL SCOPE {d}: a crate (or file) new in scope.tsv seeds its rows only when a commit in the "
                     f"range names it in a trailer `Alloclint-Scope: {d}`")
    mcx_note = ""
    if mcx_on_tree:
        rows = {k: v for k, v in new.items() if k[1] == mcxt.KIND}
        if mcx_seeding and not mcx_seed_named:
            fails.append(f"alloclint base FAIL MCX SEED: {LEDGER}/{mcxt.SUBDIR} is new in the range: its rows seed only when a commit "
                         f"names them in a trailer `Alloclint-Seed: {mcxt.KIND}`")
        rev = None if tree is None else tree_rev
        if mcx_seeding:   # the seed must BE the census: a full scan of the tree equals its rows
            files = sorted(set(mcxt.candidates(a.repo, rev)) | {k[0] for k in rows})
        else:             # the range's changed files, a new home's file and a changed row's file
            _, ch = git(a.repo, "diff", "--name-only", "-M", a.base, *( [tree_rev] if tree is not None else []), "--", "crates")
            files = {f for f in ch.split() if mcxt.in_scope(f)} | {f for f, _ in added} \
                | {k[0] for k in set(rows) | {k for k in old if k[1] == mcxt.KIND} if rows.get(k, 0) != old.get(k, 0)}
            files = sorted(files)
        cur_m, lines_m = mcxt.scan(a.repo, files, rev)
        for x in mcxt.judge(cur_m, lines_m, rows, files):
            fails.append(x.replace("alloclint FAIL MCX", "alloclint base FAIL MCX", 1))
        mcx_note = (f"; mcx-transmute: {mcx_seeded} site(s) seeded by trailer (a full scan of {len(files)} file(s) equals them)"
                    if mcx_seeding else f"; mcx-transmute: {len(files)} changed file(s) equal their rows ({sum(rows.values())} budgeted)")
    for d, n in sorted(carried.items()):
        print(f"alloclint CARRY {d} <- {dict(cmap).get(d, 'the renamed crates in their item names')}: {n} row(s) at or under the "
              f"old row's budget ({CRATE_RENAMES})")
    for x in fails:
        print(x)
    if fails:
        return 1
    seed_n = mcx_seeded if mcx_on_tree and mcx_seeding else 0   # the named seed is not the old rows rising: say it apart
    seed_n += cc_seeded
    seed_say = " + ".join(x for x in (f"the {mcx_seeded} mcx-transmute seed rows" if mcx_on_tree and mcx_seeding and mcx_seeded else "",
                                      f"the {cc_seeded} charged-caller seed rows (D4505, {len(cc_files)} file(s))" if cc_seeded else "") if x)
    print(f"alloclint base: PASS: {'every row already in the ledger at or under its budget' if seeded_dirs else 'no row rises but the named TYPE-MENTION catch-up' if caught else f'no row rises but the user-approved add(s) (up to {approved} site(s))' if approved else 'the ledger only shrinks'} "
          f"({sum(old.values())} -> {sum(new.values()) - seed_n} sites{f' + {seed_say}' if seed_say else ''})"
          + (f"; {seeded} site(s) seeded for {len(seeded_dirs)} crate(s) new in scope ({', '.join(sorted(seeded_dirs))}), named by trailer"
             if seeded_dirs else "")
          + (f"; {len(added)} new home(s) named by trailer" if added else "")
          + (f"; {sum(carried.values())} row(s) carried across {len([d for d in carried if d != '(items)'])} crate rename(s) "
             f"({CRATE_RENAMES})" if carried else "")
          + (f"; {len(moved_homes)} home(s) moved with their file" if moved_homes else "")
          + (f"; {na_added} NOALLOC row(s) named by trailer" if na_added else "")
          + (f"; {len(u_rows)} UNREACHED row(s) proven" if u_rows else "")
          + (f"; TYPE-MENTION catch-up: {caught} ir-only site(s) for type rows the range removes, named by trailer" if caught else "")
          + mcx_note)
    return 0


def cmd_selftest(_):
    import json
    with tempfile.TemporaryDirectory() as t:
        def msg(f, l, kind, item, code="clippy::disallowed_methods"):
            return json.dumps({"reason": "compiler-message", "message": {
                "code": {"code": code}, "level": "warning", "message": f"use of a disallowed method `{item}`",
                "spans": [{"is_primary": True, "file_name": f, "line_start": l, "column_start": 1}],
                "children": [{"message": f"alloclint:{kind}"}]}})
        f = CRATE + "/src/a.rs"
        js = os.path.join(t, "c.json")
        open(js, "w").write("\n".join([msg(f, 3, "growth", "std::vec::Vec::push"), msg(f, 9, "growth", "std::vec::Vec::push"),
                                       msg(f, 4, "type", "std::vec::Vec", "clippy::disallowed_types")]) + "\n")
        # seeding a crate new in scope (no ledger dir) from IR rows alone (an empty clippy replay, alloclint#3286) is refused
        c2 = "crates/backend/access/zz"
        irm = os.path.join(t, "misses.tsv")
        open(irm, "w").write("file\tline\tfn\tcallee\titem\n" + "".join(f"{c2}/src/b.rs\t{7 + i}\tf\tg\talloc::fmt::format\n"
                                                                     for i in range(SEED_IR_ONLY_MIN)))
        assert cmd_write(argparse.Namespace(findings=js, ir_misses=irm, root=t, crate=[CRATE, c2])) == 2, "an IR-only seed is refused"
        assert not os.path.isdir(os.path.join(t, LEDGER, crate_key(c2))), "nothing written"
        A = argparse.Namespace(findings=js, ir_misses=None, root=t, crate=[CRATE])
        assert cmd_write(A) == 0
        assert not os.path.exists(os.path.join(t, LEDGER, "README")), "write writes no README (TD-203: a generated file collided)"
        assert cmd_check(A) == 0, "equal counts pass"
        open(js, "a").write(msg(f, 12, "growth", "std::vec::Vec::push") + "\n")
        assert cmd_check(A) == 1, "a new push fails"
        open(js, "w").write(msg(f, 3, "growth", "std::vec::Vec::push") + "\n" + msg(f, 4, "type", "std::vec::Vec", "clippy::disallowed_types") + "\n")
        assert cmd_check(A) == 1, "a removed push leaves a stale budget"
        assert cmd_write(A) == 0 and cmd_check(A) == 0, "write lowers it"
        # write keeps a ledger file's '#' notes verbatim (a user-approved add's note), and --file rewrites only that file
        lf = os.path.join(t, LEDGER, crate_key(CRATE), ledger_name(f))
        note = "# USER-APPROVED ADD (selftest): the note stays"
        body = open(lf).read().split("\n", 1)
        open(lf, "w").write(body[0] + "\n" + note + "\n" + body[1])
        g = CRATE + "/src/b.rs"
        open(js, "a").write(msg(g, 5, "growth", "std::vec::Vec::push") + "\n")
        assert cmd_write(A) == 0 and note in open(lf).read().splitlines()[1], "a full crate write keeps the note"
        lg = os.path.join(t, LEDGER, crate_key(CRATE), ledger_name(g))
        assert "std::vec::Vec::push\t1" in open(lg).read()
        open(js, "a").write(msg(f, 20, "growth", "std::vec::Vec::push") + "\n" + msg(g, 6, "growth", "std::vec::Vec::push") + "\n")
        F = argparse.Namespace(findings=js, ir_misses=None, root=t, crate=[CRATE], file=[f])
        assert cmd_write(F) == 0
        assert "std::vec::Vec::push\t2" in open(lf).read() and note in open(lf).read(), "--file rewrites its file, note kept"
        assert "std::vec::Vec::push\t1" in open(lg).read(), "--file leaves the crate's other files untouched"
        open(js, "w").write(msg(f, 4, "type", "std::vec::Vec", "clippy::disallowed_types") + "\n" + msg(g, 5, "growth", "std::vec::Vec::push") + "\n")
        assert cmd_write(F) == 0 and open(lf).read().splitlines() == ["path\t" + f, note], "a file whose rows all go keeps its note"
        open(js, "w").write(msg(f, 3, "growth", "std::vec::Vec::push") + "\n" + msg(f, 4, "type", "std::vec::Vec", "clippy::disallowed_types") + "\n")
        os.remove(lg)
        open(lf, "w").write("\n".join(x for x in open(lf).read().splitlines() if x != note) + "\n")
        assert cmd_write(A) == 0 and cmd_check(A) == 0 and note not in open(lf).read(), "back to the one push, no note"
        # UNREACHED + NOALLOC (TD-197 amendment, memacct GO 2026-10-15T20:3xZ)
        import contextlib as _cl, io as _io

        def chk(ns):
            buf = _io.StringIO()
            with _cl.redirect_stdout(buf):
                rc_ = cmd_check(ns)
            return rc_, buf.getvalue()
        src_f = os.path.join(t, f)
        os.makedirs(os.path.dirname(src_f), exist_ok=True)
        open(src_f, "w").write("fn live() {\n    let a = k.clone();\n    v.push(1);\n    let t: Vec<u8> = x;\n}\n"
                               "fn only_tests() {\n    let x = 1;\n    w.push(2);\n}\n")
        base_js = open(js).read()
        open(js, "a").write(msg(f, 8, "growth", "std::vec::Vec::push") + "\n")
        assert cmd_check(A) == 1, "a new push in a function nobody listed fails NEW"
        up = os.path.join(t, unreached_mod.LIST)
        os.makedirs(os.path.dirname(up), exist_ok=True)
        open(up, "w").write(f"# selftest\n{f}\tonly_tests\tx\n")
        rc_, o_ = chk(A)
        assert rc_ == 0 and "UNREACHED 1 " in o_, ("a finding in a listed function is UNREACHED", o_)
        open(js, "a").write(msg(f, 2, "method", "std::clone::Clone::clone") + "\n")
        assert cmd_check(A) == 1, "a clone with no IR proof is debt (NEW)"
        nat = os.path.join(t, "noalloc.tsv")
        open(nat, "w").write("file\tline\tkind\titem\tcount\ttext\n"
                             f"{f}\t2\tmethod\tstd::clone::Clone::clone\t1\t{findings_mod.line_key('    let a = k.clone();')}\n")
        AN = argparse.Namespace(findings=js, ir_misses=None, root=t, crate=[CRATE], noalloc=nat)
        rc_, o_ = chk(AN)
        assert rc_ == 0 and "NOALLOC 1 " in o_, ("the IR's proof makes it NOALLOC", o_)
        assert cmd_write(AN) == 0 and os.path.exists(os.path.join(t, LEDGER, crate_key(CRATE), NOALLOC_FILE)), "write --noalloc keeps .noalloc"
        rc_, o_ = chk(A)
        assert rc_ == 0 and "NOALLOC 1 " in o_, ("the clippy leg reads the ledger's .noalloc", o_)
        # a landing that moves the unchanged line (a line inserted above it) keeps its proof: rows are the line's TEXT
        js_keep, src_keep = open(js).read(), open(src_f).read()

        def shifted(text):
            out = []
            for ln_ in text.splitlines():
                d_ = json.loads(ln_)
                sp_ = d_["message"]["spans"][0]
                if sp_["file_name"] == f:
                    sp_["line_start"] += 1
                out.append(json.dumps(d_))
            return "\n".join(out) + "\n"
        open(js, "w").write(shifted(js_keep))
        open(src_f, "w").write("// a new first line\n" + src_keep)
        open(up, "w").write(f"# selftest\n{f}\tonly_tests\tx\n")
        rc_, o_ = chk(A)
        assert rc_ == 0 and "NOALLOC 1 " in o_, ("a moved, unchanged line keeps NOALLOC", o_)
        open(nat, "w").write("file\tline\tkind\titem\tcount\ttext\n"
                             f"{f}\t3\tmethod\tstd::clone::Clone::clone\t1\t{findings_mod.line_key('    let a = k.clone();')}\n")
        rc_, o_ = chk(AN)
        assert rc_ == 0 and "NOALLOC-UNPROVEN" not in o_, ("the IR's proof at the moved line proves the ledger's row", o_)
        open(js, "w").write(js_keep)
        open(src_f, "w").write(src_keep)
        open(nat, "w").write("file\tline\tkind\titem\tcount\ttext\n"
                             f"{f}\t2\tmethod\tstd::clone::Clone::clone\t1\t{findings_mod.line_key('    let a = k.clone();')}\n")
        src_t = open(src_f).read()
        open(src_f, "w").write(src_t.replace("let a = k.clone();", "let a = k.clone(); // edited"))
        assert cmd_check(A) == 1, "an edited line is debt again (NEW) until a full run re-derives it"
        open(nat, "w").write("file\tline\tkind\titem\tcount\ttext\n")   # the IR leg on the edited tree proves nothing there
        rc_, o_ = chk(AN)
        assert rc_ == 1 and f"NOALLOC-UNPROVEN {f}:2 method std::clone::Clone::clone" in o_, ("the IR leg reports a ledger row it no longer proves", o_)
        assert cmd_write(A) == 0 and not os.path.exists(os.path.join(t, LEDGER, crate_key(CRATE), NOALLOC_FILE)), \
            "a clippy-only write drops the edited line's row"
        assert "std::clone::Clone::clone\t1" in open(lf).read(), "and its finding is a row"
        # COLD-TEXT by TYPE: a Cold-tagged Box::new (MISTAGGED by name) whose row the IR proved boxes a PgError is COLD-TEXT
        src_t = open(src_f).read()
        open(src_f, "w").write(src_t + "fn err() {\n    return Box::new(e); // alloc: Cold\n}\n")
        open(js, "a").write(msg(f, 11, "method", "std::boxed::Box::new") + "\n")
        rc_, o_ = chk(A)
        assert rc_ == 1 and "MISTAGGED 1" in o_, ("a Cold tag on Box::new is MISTAGGED debt by name", o_)
        open(nat, "w").write("file\tline\tkind\titem\tcount\ttext\tclass\n"
                             f"{f}\t11\tmethod\tstd::boxed::Box::new\t1\t"
                             f"{findings_mod.line_key('    return Box::new(e); // alloc: Cold')}\tCOLD-TEXT\n")
        rc_, o_ = chk(AN)
        assert "MISTAGGED 0" in o_ and "COLD-TEXT 1 " in o_ and "Box::new" not in o_.split("alloclint:")[0], ("the IR's PgError payload makes it COLD-TEXT", o_)
        open(js, "w").write(base_js)
        os.remove(up)
        os.remove(src_f)
        assert cmd_write(A) == 0 and cmd_check(A) == 0, "back to the one push"
        # --lower-only: a NEW site does not raise its row; a removed one lowers it
        open(js, "a").write(msg(f, 7, "growth", "std::vec::Vec::push") + "\n")
        AL = argparse.Namespace(findings=js, ir_misses=None, root=t, crate=[CRATE], noalloc=None, lower_only=True)
        rc_w = cmd_write(AL)
        assert rc_w == 0 and cmd_check(A) == 1 and "std::vec::Vec::push\t1" in open(lf).read(), ("a NEW row stays NEW", rc_w, open(lf).read())
        open(js, "w").write(base_js)
        assert cmd_write(A) == 0 and cmd_check(A) == 0, "back to the one push"
        # the S1 leg (ALLOCLINT_BASE set): a renames.tsv row the range adds must name a finding of its new item on its line
        os.makedirs(os.path.dirname(os.path.join(t, RENAMES)), exist_ok=True)
        hdr = "file\tline\told_kind\told_item\tnew_kind\tnew_item\tcount\tlane\treason\n"
        open(os.path.join(t, RENAMES), "w").write(hdr + f"{f}\t3\tgrowth\tstd::vec::Vec::insert\tgrowth\tstd::vec::Vec::push\t1\tx\ty\n")
        os.environ["ALLOCLINT_BASE"] = "no-such-base"
        try:
            assert cmd_check(A) == 0, "a rename row on its new item's finding line passes (printed)"
            open(os.path.join(t, RENAMES), "w").write(hdr + f"{f}\t5\tgrowth\tstd::vec::Vec::insert\tgrowth\tstd::vec::Vec::push\t1\tx\ty\n")
            assert cmd_check(A) == 1, "a rename row naming no finding on its line fails"
            # an ir-only rename row (planmem 15:46:58Z, main 15:47:11Z): not judged without --ir-misses (S1 runs clippy only, as the
            # STALE loop skips ir-only rows); with the IR leg's misses it names its line or fails, as any row
            import contextlib, io
            open(os.path.join(t, RENAMES), "w").write(hdr + f"{f}\t7\ttype\tstd::vec::Vec\tir-only\talloc::rc::Rc::from\t1\tx\ty\n")
            buf = io.StringIO()
            with contextlib.redirect_stdout(buf):
                assert cmd_check(A) == 0, "an ir-only rename row is not judged without the IR leg"
            assert f"alloclint RENAME {f}:7 type std::vec::Vec -> ir-only alloc::rc::Rc::from x1" in buf.getvalue() and "FAIL" not in buf.getvalue(), buf.getvalue()
            for at, named in ((8, False), (7, True)):
                open(irm, "w").write(f"file\tline\tfn\tcallee\titem\n{f}\t{at}\tf\tg\talloc::rc::Rc::from\n")
                buf = io.StringIO()
                with contextlib.redirect_stdout(buf):
                    cmd_check(argparse.Namespace(findings=js, ir_misses=irm, root=t, crate=[CRATE]))
                assert (f"alloclint FAIL RENAME {f}:7 ir-only alloc::rc::Rc::from" in buf.getvalue()) != named, (at, buf.getvalue())
        finally:
            del os.environ["ALLOCLINT_BASE"]
            os.remove(os.path.join(t, RENAMES))
        # a charged line (wave 2 R1 + R5) is tracked: its finding is no site, so a new one passes and a row never names it
        os.makedirs(os.path.join(t, CRATE, "src"), exist_ok=True)
        open(os.path.join(t, f), "w").write("\n" * 19 + "v.push(x); // alloc: Exec charged w.stmt_owner()\n"
                                            "v.push(y); // alloc: charged slot.owner\n" + "v.push(z); // alloc: Exec\n")
        open(js, "a").write(msg(f, 20, "growth", "std::vec::Vec::push") + "\n" + msg(f, 21, "growth", "std::vec::Vec::push") + "\n")
        assert cmd_check(A) == 0, "a charged line is tracked"
        open(js, "a").write(msg(f, 22, "growth", "std::vec::Vec::push") + "\n")
        assert cmd_check(A) == 1, "a plain lifetime tag is not a charge"
        # D4505 (USER GO via main 2026-10-15T05:52:10Z): `charged caller` is a home (HC) only inside a pass-through writer the
        # alloclint clippy.toml lists (its callers' call sites are the rows); elsewhere the line's findings are MISTAGGED debt
        import contextlib
        import io
        CC_SRC = ("fn put(out: &mut Vec<u8>, v: u8) {\n    out.push(v); // alloc: Exec charged caller\n}\n\n"
                  "fn enc(out: &mut Vec<u8>) {\n    out.push(1); // alloc: Exec charged caller\n"
                  "    out.push(2); // alloc: Node charged caller (the caller charges it)\n}\n")
        CC_TOML = 'disallowed-methods = [\n  { path = "lowerv4::a::put", reason = "alloclint:growth" },\n' \
                  '  { path = "std::vec::Vec::push", reason = "alloclint:growth" },\n]\n'
        lw = homes_mod.listed_writers(CC_TOML + '{ path = "x_crate::m::T::f", reason = "alloclint:method" }\n'
                                      '{ path = "x_crate::g", reason = "alloclint:growth" }\n{ path = "x_crate::h", reason = "other" }\n')
        assert lw == {("lowerv4", ("a",), None, "put"), ("x_crate", ("m",), "T", "f"), ("x_crate", (), None, "g")}, \
            f"listed_writers: workspace paths with an alloclint reason, impl type by case: {lw}"
        assert homes_mod.crate_mods("crates/a/b-c/src/lib.rs") == ("b_c", ()) and homes_mod.crate_mods("x/src/m/mod.rs") == ("x", ("m",)) \
            and homes_mod.crate_mods("x/src/m/n.rs") == ("x", ("m", "n")) and homes_mod.crate_mods("x/build.rs") is None, "crate_mods"
        assert homes_mod.caller_lines(f, CC_SRC, lw) == [6, 7], "the unlisted fn's two caller lines, not the listed writer's"
        assert homes_mod.caller_lines("crates/other/src/a.rs", CC_SRC, lw) == [2, 6, 7], "another crate's `put` is not listed"
        os.makedirs(os.path.dirname(os.path.join(t, homes_mod.CLIPPY)), exist_ok=True)
        open(os.path.join(t, homes_mod.CLIPPY), "w").write(CC_TOML)
        open(os.path.join(t, f), "w").write(CC_SRC)
        hm = homes_mod.Homes(t, None)
        assert hm.of(f, 2) == "HC" and hm.of(f, 6) is None and hm.of(f, 7) is None and hm.cold(f, 6, "std::vec::Vec::push") == "MISTAGGED" \
            and hm.cold(f, 2, "std::vec::Vec::push") is None, "Homes: HC inside the listed writer, MISTAGGED outside"
        open(js, "w").write("\n".join(msg(f, n, "growth", "std::vec::Vec::push") for n in (2, 6, 7))
                            + "\n" + msg(f, 4, "type", "std::vec::Vec", "clippy::disallowed_types") + "\n")
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            rc = cmd_check(A)
        assert rc == 1 and "FAIL NEW " + f + ":6,7 growth std::vec::Vec::push: 2 site(s), budget 1" in out.getvalue() \
            and "MISTAGGED 2 (still debt)" in out.getvalue(), "an unlisted `charged caller` line is MISTAGGED debt: " + out.getvalue()
        os.remove(os.path.join(t, homes_mod.CLIPPY))
        # COLD-TEXT (USER RULING 2026-10-14T18:52:43Z): Cold-tagged text is no site; a Cold tag on a non-text item stays debt
        # (MISTAGGED), on an item that is text only for some receivers too (COLD-UNPROVEN); Exec-tagged text stays debt
        import contextlib
        import io
        open(js, "w").write(msg(f, 3, "growth", "std::vec::Vec::push") + "\n" + msg(f, 4, "type", "std::vec::Vec", "clippy::disallowed_types") + "\n")
        open(os.path.join(t, f), "w").write("\n" * 29 + 'refuse!(format!("no {}", x)) // alloc: Cold\n'
                                            'r.push_str(&e.to_string()); // alloc: Cold\n' + "v.push(1); // alloc: Cold\n"
                                            "s.to_owned() // alloc: Cold\n" + 'format!("x") // alloc: Exec\n')
        assert cmd_check(A) == 0, "the base before the Cold lines"
        open(js, "a").write(msg(f, 30, "macro", "std::format") + "\n" + msg(f, 31, "growth", "std::string::String::push_str") + "\n"
                            + msg(f, 31, "method", "std::string::ToString::to_string") + "\n")
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            rc = cmd_check(A)
        assert rc == 0 and "COLD-TEXT 3 (named class, not debt)" in out.getvalue(), "Cold-tagged text is no site: " + out.getvalue()
        open(js, "a").write(msg(f, 32, "growth", "std::vec::Vec::push") + "\n")
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            rc = cmd_check(A)
        assert rc == 1 and "FAIL NEW " + f + ":3,32 growth std::vec::Vec::push" in out.getvalue() and "MISTAGGED 1 (still debt)" \
            in out.getvalue() and "NOTE" not in out.getvalue(), "a Cold tag on a Vec push stays debt, counted MISTAGGED: " + out.getvalue()
        open(js, "a").write(msg(f, 33, "method", "std::borrow::ToOwned::to_owned") + "\n")
        with contextlib.redirect_stdout(io.StringIO()):
            assert cmd_write(A) == 0
        txt = open(os.path.join(t, LEDGER, "lowerv4", ledger_name(f))).read()
        assert "std::format" not in txt and "push_str" not in txt and "growth\tstd::vec::Vec::push\t2" in txt \
            and "method\tstd::borrow::ToOwned::to_owned\t1" in txt, "write: no COLD-TEXT row; MISTAGGED / COLD-UNPROVEN rows: " + txt
        assert not os.path.exists(os.path.join(t, LEDGER, "lowerv4", ".columns")), "a --crate write records no .columns"
        open(js, "a").write(msg(f, 34, "macro", "std::format") + "\n")
        assert cmd_check(A) == 1, "an Exec-tagged format! is debt (only a Cold tag names the class)"
        with contextlib.redirect_stdout(io.StringIO()):
            assert cmd_write(argparse.Namespace(findings=js, ir_misses=None, root=t, crate=[])) == 0
        cols = open(os.path.join(t, LEDGER, "lowerv4", ".columns")).read()
        assert "debt\t4\ntype-mention\t1\ncold-text\t3\ncold-unproven\t1\nmistagged\t1\nrefcount-clone\t0\n" in cols, "a full write records the columns: " + cols
        assert read_ledger_dir(os.path.join(t, LEDGER), {"lowerv4"})[(f, "growth", "std::vec::Vec::push")] == 2, ".columns is not a row"
        import shutil
        for name in os.listdir(os.path.join(t, LEDGER)):   # (the full write wrote every scope.tsv crate)
            if name not in ("lowerv4", "README"):
                shutil.rmtree(os.path.join(t, LEDGER, name))
        open(js, "w").write(msg(f, 3, "growth", "std::vec::Vec::push") + "\n" + msg(f, 4, "type", "std::vec::Vec", "clippy::disallowed_types") + "\n")
        with contextlib.redirect_stdout(io.StringIO()):
            assert cmd_write(A) == 0
        assert os.path.exists(os.path.join(t, LEDGER, "lowerv4", ".columns")), "a --crate write keeps the last .columns"
        os.remove(os.path.join(t, f))
        # a crate scoped by file (scope.tsv's 4th column, main 19:42:45Z (iv)): only its listed files are counted
        cp = "crates/backend/commands/copy"
        if cp in findings_mod.SCOPE_FILES:
            cj = os.path.join(t, "cp.json")
            open(cj, "w").write(msg(cp + "/src/text_pgrc.rs", 5, "growth", "std::vec::Vec::push") + "\n" + msg(cp + "/src/from.rs", 7, "growth", "std::vec::Vec::push") + "\n")
            C = argparse.Namespace(findings=cj, ir_misses=None, root=t, crate=[cp])
            with contextlib.redirect_stdout(io.StringIO()):
                assert cmd_write(C) == 0
            rows = read_ledger_dir(os.path.join(t, LEDGER), {crate_key(cp)})
            assert set(k[0] for k in rows) == {cp + "/src/text_pgrc.rs"}, "only the listed files of a file-scoped crate: %s" % rows
            shutil.rmtree(os.path.join(t, LEDGER, crate_key(cp)))
        # a cache_scope_for(owner, kind) .. finish() guard is a home span (main 2026-10-14T20:21:07Z (1)); a boxed error value on
        # a Cold line is COLD-TEXT (20:21:07Z (2), text_hosts.rs's 53200 path)
        hp = os.path.join(t, HOMES)
        os.makedirs(os.path.dirname(hp), exist_ok=True)
        open(hp, "w").write(f"{f}\tput@scope\tH4\tx\n")
        os.makedirs(os.path.join(t, CRATE, "src"), exist_ok=True)
        open(os.path.join(t, f), "w").write("fn put() {\n    let g = memledger::arena::cache_scope_for(o, k);\n    v.push(1);\n"
                                            "    g.finish();\n    v.push(2);\n    Err(Box::new(PgError::error(m))) // alloc: Cold\n}\n")
        open(js, "w").write(msg(f, 3, "growth", "std::vec::Vec::push") + "\n" + msg(f, 5, "growth", "std::vec::Vec::push") + "\n"
                            + msg(f, 6, "method", "std::boxed::Box::new") + "\n")
        cur, _, _, cold = current(js, None, [CRATE], t)
        assert dict(cur) == {(f, "growth", "std::vec::Vec::push"): 1} and [x[4] for x in cold] == ["COLD-TEXT"], \
            "cache_scope_for span + boxed error: %s %s" % (dict(cur), cold)
        os.remove(hp)
        os.remove(os.path.join(t, f))
        # a boxed error held in a variable is COLD-TEXT when the line names its type by the turbofish (main
        # 2026-10-15T08:00:43Z); without the type, or with a non-error type, it stays MISTAGGED
        bx = "std::boxed::Box::new"
        for line, want in (("    stash_error(Box::<PgError>::new(e)); // alloc: Cold", "COLD-TEXT"),
                           ("    Box::<types_error::PgError>::new(e.with_sqlstate(code)) // alloc: Cold", "COLD-TEXT"),
                           ("    return Err(Box::< TableBuildError >::new(e)); // alloc: Cold", "COLD-TEXT"),
                           ("    stash_error(Box::new(e)); // alloc: Cold", "MISTAGGED"),
                           ("    Box::<Vec<u8>>::new(v) // alloc: Cold", "MISTAGGED"),
                           ("    Box::<ErrorKind>::new(k) // alloc: Cold", "MISTAGGED")):
            assert homes_mod.cold_class(line, bx) == want, "turbofish error box: %r -> %s" % (line, homes_mod.cold_class(line, bx))
        # TYPE-MENTION (USER RULING via main 2026-10-14T23:20:10Z): a type finding on a line with no method / growth / macro
        # finding is no row and no debt, counted beside the debt; a mixed line (type + growth) stays debt, both rows
        def ty(l, item="std::vec::Vec"):
            return msg(f, l, "type", item, "clippy::disallowed_types") + "\n"
        open(os.path.join(t, f), "w").write("\n" * 49 + "struct S { v: Vec<u8> }\n" + "let v: Vec<u8> = Vec::with_capacity(n);\n"
                                            + "x: Vec<u8>, // alloc: Cold\n" + "e: String, // alloc: Cold\n" + "v.push(1);\n")
        open(js, "w").write(ty(50) + ty(51) + msg(f, 51, "growth", "std::vec::Vec::with_capacity") + "\n" + ty(52)
                            + ty(53, "std::string::String") + msg(f, 54, "growth", "std::vec::Vec::push") + "\n")
        cur, _, n, cold = current(js, None, [CRATE], t)
        assert dict(cur) == {(f, "type", "std::vec::Vec"): 1, (f, "growth", "std::vec::Vec::with_capacity"): 1,
                             (f, "growth", "std::vec::Vec::push"): 1}, "a mixed line keeps its type row, a type-only line has none: %s" % dict(cur)
        assert sorted((x[1], x[4]) for x in cold) == [(50, "TYPE-MENTION"), (52, "TYPE-MENTION"), (53, "COLD-TEXT")], \
            "type-only lines are TYPE-MENTION (a Cold Vec too: no MISTAGGED debt); a Cold String stays COLD-TEXT: %s" % cold
        assert n == 6, "n counts the mentions (a crate of mentions only is judged, not 'no finding'): %d" % n
        with contextlib.redirect_stdout(io.StringIO()):
            assert cmd_write(A) == 0
        txt = open(os.path.join(t, LEDGER, "lowerv4", ledger_name(f))).read()
        assert "type\tstd::vec::Vec\t1\n" in txt and "type\tstd::string::String" not in txt, "the ledger holds the mixed line's type only: " + txt
        open(js, "a").write(ty(55) + ty(56, "std::collections::HashMap"))
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            rc = cmd_check(A)
        assert rc == 0 and "debt) in 1 files of 1 crate(s), each at its budget; TYPE-MENTION 4 (" in out.getvalue(), \
            "a new type-only line is no NEW, and the column stands beside the debt: " + out.getvalue()
        open(js, "a").write(ty(57) + msg(f, 57, "method", "std::vec::Vec::new") + "\n")
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            rc = cmd_check(A)
        assert rc == 1 and "FAIL NEW " + f + ":51,57 type std::vec::Vec" in out.getvalue() \
            and "FAIL NEW " + f + ":57 method std::vec::Vec::new" in out.getvalue(), "a new mixed line is NEW, its type too: " + out.getvalue()
        open(js, "w").write(ty(50) + ty(52))
        with contextlib.redirect_stdout(io.StringIO()):
            assert cmd_write(A) == 0, "a crate whose findings are all mentions is written (not 'no finding')"
        assert os.path.exists(os.path.join(t, LEDGER, "lowerv4", ".empty")) and cmd_check(A) == 0, "mentions only: judged at zero"
        # REFCOUNT-CLONE (USER GO via main 2026-10-15T11:46:31Z): a Clone::clone inside a clone_on_ref_ptr span (the compiler
        # resolved its receiver to an Rc / Arc / Weak) alone on its line is no row and no debt; beside a type or another clone
        # whose receiver is not one, every finding on the line stays debt
        def cl(l, col):
            return json.dumps({"reason": "compiler-message", "message": {
                "code": {"code": "clippy::disallowed_methods"}, "level": "warning", "message": "use of a disallowed method `std::clone::Clone::clone`",
                "spans": [{"is_primary": True, "file_name": f, "line_start": l, "column_start": col}],
                "children": [{"message": "alloclint:method"}]}}) + "\n"

        def rp(l, c0, c1, l1=None):
            return json.dumps({"reason": "compiler-message", "message": {
                "code": {"code": "clippy::clone_on_ref_ptr"}, "level": "warning", "message": "using `.clone()` on a ref-counted pointer",
                "spans": [{"is_primary": True, "file_name": f, "line_start": l, "column_start": c0, "line_end": l1 or l, "column_end": c1}],
                "children": []}}) + "\n"
        open(os.path.join(t, f), "w").write("\n" * 70 + "let a = self.a.clone();\n" + "let v = self.v.clone();\n"
                                            + "let b: Arc<T> = a.clone();\n" + "(a.clone(), v.clone())\n" + "let c = self\n" + "    .a.clone();\n"
                                            + "let s = m[k.clone()].clone();\n")
        # (line 77: the Arc clone's span holds the String clone of its index; only the clone that closes the span is the Arc's)
        open(js, "w").write(cl(71, 16) + rp(71, 9, 23) + cl(72, 16) + cl(73, 20) + rp(73, 18, 27) + ty(73, "std::sync::Arc")
                            + cl(74, 4) + rp(74, 2, 11) + cl(74, 14) + cl(76, 8) + rp(75, 9, 15, 76)
                            + cl(77, 13) + cl(77, 22) + rp(77, 9, 29))
        cur, _, n, cold = current(js, None, [CRATE], t)
        assert sorted((x[1], x[4]) for x in cold if x[4] == "REFCOUNT-CLONE") == [(71, "REFCOUNT-CLONE"), (76, "REFCOUNT-CLONE")], \
            "an Arc clone alone on its line (its receiver on the line before too) is REFCOUNT-CLONE: %s" % cold
        # REFCOUNT-CLONE-INLINE (USER RULING via memacct 2026-10-15T18:5xZ): the Arc clones beside a type (73), beside a Vec
        # clone (74) and around a String clone (77) are no debt either; the Vec / String clones stay debt, and the type on 73
        # is a TYPE-MENTION (its line's only allocation finding was the Arc clone)
        assert sorted((x[1], x[4]) for x in cold if x[4] == "REFCOUNT-CLONE-INLINE") == [
            (73, "REFCOUNT-CLONE-INLINE"), (74, "REFCOUNT-CLONE-INLINE"), (77, "REFCOUNT-CLONE-INLINE")], cold
        assert [(x[1], x[4]) for x in cold if x[4] == "TYPE-MENTION" and x[1] == 73] == [(73, "TYPE-MENTION")], cold
        assert dict(cur) == {(f, "method", "std::clone::Clone::clone"): 3}, \
            "the Vec clones (72, 74) and the String clone inside the Arc clone's span (77) stay debt: %s" % dict(cur)
        assert n == 9, "n counts the named classes too: %d" % n
        assert "REFCOUNT-CLONE-INLINE 3 (" in cold_say(cold), cold_say(cold)
        assert findings_mod.refcount_findings([(f, 77, 13, "method", "std::clone::Clone::clone")], {f: {(77, 29)}}) == set(), \
            "a clone inside an Arc clone's span that does not close it is no REFCOUNT-CLONE"
        assert "REFCOUNT-CLONE 2 (" in cold_say(cold), cold_say(cold)
        os.remove(os.path.join(t, f))
        open(js, "w").write(msg(f, 3, "growth", "std::vec::Vec::push") + "\n" + ty(4))
        with contextlib.redirect_stdout(io.StringIO()):
            assert cmd_write(A) == 0
        open(js, "w").write(json.dumps({"reason": "compiler-message", "message": {"level": "error", "message": "x", "rendered": "error[E0425]"}}) + "\n")
        try:
            cmd_check(A)
            raise AssertionError("a failed build must not judge")
        except SystemExit as e:
            assert e.code == 2
        # base: a repo whose ledger raises a row fails, one that lowers passes
        assert os.path.isfile(os.path.join(t, LEDGER, "lowerv4", ledger_name(f))), "one directory per crate"
        open(js, "w").write(msg(f, 3, "growth", "std::vec::Vec::push") + "\n" + msg(f, 4, "type", "std::vec::Vec", "clippy::disallowed_types") + "\n")
        assert cmd_check(argparse.Namespace(findings=js, ir_misses=None, root=t, crate=[CRATE, "crates/gravity/kernels"])) == 2, \
            "a crate with no ledger directory cannot be judged"
        r = os.path.join(t, "repo")
        os.makedirs(os.path.join(r, LEDGER, "lowerv4"))
        subprocess.run(["git", "init", "-q", r], check=True)
        p = os.path.join(r, LEDGER, "lowerv4", ledger_name(f))
        open(p, "w").write(f"path\t{f}\ngrowth\tstd::vec::Vec::push\t2\n")
        open(os.path.join(r, LEDGER, "README"), "w").write(LEDGER_DOC)   # an older base's README: still skipped by the readers
        env = ["-c", "user.email=x@y", "-c", "user.name=x"]
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-qm", "seed"], check=True)
        open(p, "w").write(f"path\t{f}\ngrowth\tstd::vec::Vec::push\t3\n")
        B = argparse.Namespace(repo=r, base="HEAD", tree=None)
        assert cmd_base(B) == 1, "a raise fails"
        open(p, "w").write(f"path\t{f}\ngrowth\tstd::vec::Vec::push\t1\n")
        assert cmd_base(B) == 0, "a lower passes"
        open(p, "w").write(f"path\t{f}\ngrowth\tstd::vec::Vec::push\t1\nmacro\tstd::vec\t1\n")
        assert cmd_base(B) == 1, "an added row fails"
        assert cmd_base(argparse.Namespace(repo=r, base="nosuchrev", tree=None)) == 2, "a missing rev cannot judge"
        open(p, "w").write(f"path\t{f}\ngrowth\tstd::vec::Vec::push\t1\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "lower"], check=True)
        assert cmd_base(argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD")) == 0, "a committed lower passes"
        # check.sh's staged mode passes a tree object (git write-tree), not a commit
        open(p, "w").write(f"path\t{f}\ngrowth\tstd::vec::Vec::push\t2\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        wt = subprocess.run(["git", "-C", r, "write-tree"], capture_output=True, text=True, check=True).stdout.strip()
        assert cmd_base(argparse.Namespace(repo=r, base="HEAD", tree=wt)) == 1, "a staged raise fails"
        open(p, "w").write(f"path\t{f}\ngrowth\tstd::vec::Vec::push\t1\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        wt = subprocess.run(["git", "-C", r, "write-tree"], capture_output=True, text=True, check=True).stdout.strip()
        assert cmd_base(argparse.Namespace(repo=r, base="HEAD", tree=wt)) == 0, "a staged tree at the same budget passes"
        hp = os.path.join(r, HOMES)
        os.makedirs(os.path.dirname(hp), exist_ok=True)
        open(hp, "w").write("# homes\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-qm", "homes"], check=True)
        open(hp, "a").write(f"{f}\twalk\tH4\tx\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "quiet home"], check=True)
        assert cmd_base(argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD")) == 1, "a new home without a trailer fails"
        subprocess.run(["git", "-C", r] + env + ["commit", "-q", "--amend", "-m", f"named home\n\nAlloclint-Home: {f} walk"], check=True)
        assert cmd_base(argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD")) == 0, "a new home named by trailer passes"
        # a scope extension: a crate new in scope.tsv seeds its rows when a trailer names it; a crate already in scope cannot add
        sp = os.path.join(r, SCOPE)
        open(sp, "w").write("crates/gravity/lowerv4\tgravity_lowerv4\tBOTH\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-qm", "scope"], check=True)
        nf = "crates/backend/access/pq/src/a.rs"
        os.makedirs(os.path.join(r, LEDGER, "crates.backend.access.pq"))
        open(os.path.join(r, LEDGER, "crates.backend.access.pq", ledger_name(nf)), "w").write(f"path\t{nf}\ngrowth\tstd::vec::Vec::push\t5\n")
        open(sp, "a").write("crates/backend/access/pq\tpq\tRUNTIME\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-qm", "pq joins scope"], check=True)
        assert cmd_base(argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD")) == 1, "a crate new in scope without a trailer fails"
        subprocess.run(["git", "-C", r] + env + ["commit", "-q", "--amend", "-m", "pq joins scope\n\nAlloclint-Scope: crates/backend/access/pq"], check=True)
        assert cmd_base(argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD")) == 0, "a crate new in scope named by trailer seeds its rows"
        # a seed of IR-only rows (an empty clippy replay, alloclint#3286) FAILs SEED, named or not; a small IR-only crate passes
        q2 = "crates/backend/access/pq2/src/a.rs"
        os.makedirs(os.path.join(r, LEDGER, "crates.backend.access.pq2"))
        open(os.path.join(r, LEDGER, "crates.backend.access.pq2", ledger_name(q2)), "w").write(
            f"path\t{q2}\nir-only\talloc::fmt::format\t{SEED_IR_ONLY_MIN}\n")
        open(sp, "a").write("crates/backend/access/pq2\tpq2\tRUNTIME\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-qm", "pq2 joins scope\n\nAlloclint-Scope: crates/backend/access/pq2"], check=True)
        assert cmd_base(argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD")) == 1, "an IR-only seed FAILs SEED"
        open(os.path.join(r, LEDGER, "crates.backend.access.pq2", ledger_name(q2)), "w").write(
            f"path\t{q2}\nir-only\talloc::fmt::format\t{SEED_IR_ONLY_MIN}\ntype\tstd::vec::Vec\t2\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "pq2 joins scope\n\nAlloclint-Scope: crates/backend/access/pq2"], check=True)
        assert cmd_base(argparse.Namespace(repo=r, base="HEAD~2", tree="HEAD")) == 0, "a seed with clippy-kind rows passes"
        open(os.path.join(r, LEDGER, "crates.backend.access.pq2", ledger_name(q2)), "w").write(f"path\t{q2}\nir-only\talloc::fmt::format\t1\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "pq2 small\n\nAlloclint-Scope: crates/backend/access/pq2"], check=True)
        assert cmd_base(argparse.Namespace(repo=r, base="HEAD~3", tree="HEAD")) == 0, "a small IR-only crate (gravity/dict's 1 row) passes"
        open(p, "w").write(f"path\t{f}\ngrowth\tstd::vec::Vec::push\t1\nmacro\tstd::vec\t1\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "lowerv4 adds\n\nAlloclint-Scope: crates/gravity/lowerv4"], check=True)
        assert cmd_base(argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD")) == 1, "a crate already in scope cannot add, trailer or not"
        open(p, "w").write(f"path\t{f}\ngrowth\tstd::vec::Vec::push\t1\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "back"], check=True)
        # CRATE RENAMES (USER RULING 2026-10-15 ~20:0xZ via memacct, "Carry trunk's allowances"): the tree root's
        # comment-budget-renames.tsv carries a moved crate's rows and homes when git does not pair its files
        import contextlib as _cl, io as _io

        def cbase(base="HEAD~1"):
            buf = _io.StringIO()
            with _cl.redirect_stdout(buf):
                rc_ = cmd_base(argparse.Namespace(repo=r, base=base, tree="HEAD"))
            return rc_, buf.getvalue()

        def put(rel, text):
            full = os.path.join(r, rel)
            os.makedirs(os.path.dirname(full), exist_ok=True)
            open(full, "w").write(text)
        rq = "crates/backend/access/pqr/src/a.rs"
        lq_old = os.path.join(LEDGER, "crates.backend.access.pq", ledger_name(nf))
        lq_new = os.path.join(LEDGER, "crates.backend.access.pqr", ledger_name(rq))
        put(nf, "fn f() {\n    let v = Vec::with_capacity(4);\n}\n")
        put(lq_old, f"path\t{nf}\ngrowth\tpq::put\t2\ngrowth\tstd::vec::Vec::push\t5\nmethod\tstd::vec::Vec::with_capacity\t1\n")
        put(p, f"path\t{f}\ngrowth\tstd::vec::Vec::push\t1\nmethod\tpq::helper\t1\n")   # a file outside the crate names it
        open(hp, "a").write(f"{nf}\tpqwalk\tH4\tx\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-qm", f"pq source\n\nAlloclint-Home: {nf} pqwalk"], check=True)
        # the move: no source line in common (git pairs nothing), the ledger directory, scope.tsv and homes.tsv renamed
        subprocess.run(["git", "-C", r, "rm", "-q", nf, lq_old], check=True)
        put(rq, "pub fn g(n: usize) -> R {\n    let v = Vec::with_capacity(4); // pqr\n    Ok(v) }\n")
        put(lq_new, f"path\t{rq}\ngrowth\tpqr::put\t2\ngrowth\tstd::vec::Vec::push\t5\nmethod\tstd::vec::Vec::with_capacity\t1\n")
        sp_t, hp_t = open(sp).read(), open(hp).read()
        assert "crates/backend/access/pq\tpq\t" in sp_t and f"{nf}\tpqwalk" in hp_t, (sp_t, hp_t)
        put(SCOPE, sp_t.replace("crates/backend/access/pq\tpq\t", "crates/backend/access/pqr\tpq\t"))
        put(HOMES, hp_t.replace(f"{nf}\tpqwalk", f"{rq}\tpqwalk"))
        put(p, f"path\t{f}\ngrowth\tstd::vec::Vec::push\t1\nmethod\tpqr::helper\t1\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-qm", "pq -> pqr"], check=True)
        rc_, o_ = cbase()
        assert rc_ == 1 and "FAIL SCOPE crates/backend/access/pqr" in o_ and "FAIL HOME" in o_, ("no map: a new crate in scope", o_)
        put(CRATE_RENAMES, "# new\told\ncrates/backend/access/pqr\tcrates/backend/access/pq\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-q", "--amend", "-m", "pq -> pqr, mapped"], check=True)
        rc_, o_ = cbase()
        assert rc_ == 0 and "CARRY crates/backend/access/pqr <- crates/backend/access/pq: 3 row(s)" in o_ \
            and "CARRY (items) <- the renamed crates in their item names: 1 row(s)" in o_ \
            and f"HOME MOVED {rq} pqwalk <- {nf}" in o_ and "4 row(s) carried across 1 crate rename(s)" in o_, ("map: carried", o_)
        put(lq_new, f"path\t{rq}\ngrowth\tpqr::put\t2\ngrowth\tstd::vec::Vec::push\t6\nmethod\tstd::vec::Vec::with_capacity\t1\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "pqr raises"], check=True)
        rc_, o_ = cbase("HEAD~2")
        assert rc_ == 1 and f"FAIL RAISES {rq} growth std::vec::Vec::push: 5 -> 6" in o_, ("a carried row raised", o_)
        put(lq_new, f"path\t{rq}\ngrowth\tpqr::put\t2\ngrowth\tstd::vec::Vec::push\t5\nmethod\tstd::vec::Vec::with_capacity\t1\nmacro\tstd::vec\t1\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-q", "--amend", "-am", "pqr adds\n\nAlloclint-Scope: crates/backend/access/pqr"],
                       check=True)
        rc_, o_ = cbase("HEAD~2")
        assert rc_ == 1 and f"FAIL ADDS {rq} macro std::vec = 1" in o_ and "FAIL SCOPE" not in o_, \
            ("no old row: NEW, and a carried crate is not new in scope (its trailer opens nothing)", o_)
        put(lq_new, f"path\t{rq}\ngrowth\tpqr::put\t2\ngrowth\tstd::vec::Vec::push\t5\nmethod\tstd::vec::Vec::with_capacity\t1\n")
        put(lq_old, f"path\t{nf}\ngrowth\tpq::put\t2\ngrowth\tstd::vec::Vec::push\t5\nmethod\tstd::vec::Vec::with_capacity\t1\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-q", "--amend", "-m", "pqr, old rows kept"], check=True)
        rc_, o_ = cbase("HEAD~2")
        assert rc_ == 1 and f"FAIL ADDS {rq} growth std::vec::Vec::push = 5" in o_, ("the old row still in the tree: the new one is NEW", o_)
        # a same-site item change across the move (Vec::with_capacity -> pgrc_format::try_vec, the line's fallible idiom): a
        # renames.tsv row, proven on the blobs across the move
        subprocess.run(["git", "-C", r, "rm", "-q", lq_old], check=True)
        put(rq, "pub fn g(n: usize) -> R {\n    let v = try_vec(4)?; // pqr\n    Ok(v) }\n")
        put(lq_new, f"path\t{rq}\ngrowth\tpqr::put\t2\ngrowth\tstd::vec::Vec::push\t5\nmethod\tpgrc_format::try_vec\t1\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-q", "--amend", "-m", "pqr fallible"], check=True)
        rc_, o_ = cbase("HEAD~2")
        assert rc_ == 1 and f"FAIL ADDS {rq} method pgrc_format::try_vec = 1" in o_, ("an item change with no rename row", o_)
        ren_text = open(os.path.join(r, RENAMES)).read() if os.path.exists(os.path.join(r, RENAMES)) else ""
        put(RENAMES, ren_text + f"{rq}\t2\tmethod\tstd::vec::Vec::with_capacity\tmethod\tpgrc_format::try_vec\t1\talloclint\tfallible\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-q", "--amend", "-m", "pqr fallible, renamed"], check=True)
        rc_, o_ = cbase("HEAD~2")
        assert rc_ == 0 and f"RENAME {rq}:2 (base line 2) method std::vec::Vec::with_capacity -> method pgrc_format::try_vec" in o_, \
            ("a same-site rename across the crate move", o_)
        # NOALLOC rows are the IR leg's: an added .noalloc row needs `Alloclint-Noalloc: <crate dir>`
        nap = os.path.join(r, LEDGER, crate_key(CRATE), NOALLOC_FILE)
        put(os.path.relpath(nap, r), f"# x\nfile\tline\tkind\titem\tcount\ttext\n{f}\t3\tmethod\tstd::clone::Clone::clone\t1\tabc\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-qm", "noalloc"], check=True)
        rc_, o_ = cbase()
        assert rc_ == 1 and "FAIL NOALLOC crates/gravity/lowerv4: 1 .noalloc row(s)" in o_, ("a .noalloc row with no trailer", o_)
        subprocess.run(["git", "-C", r] + env + ["commit", "-q", "--amend", "-m", f"noalloc\n\nAlloclint-Noalloc: {CRATE}"], check=True)
        rc_, o_ = cbase()
        assert rc_ == 0 and "1 NOALLOC row(s) named by trailer" in o_, ("named by trailer", o_)
        # UNREACHED: every unreached.tsv row is proven again on the tree (a server binary's package and its dependencies)
        put("Cargo.toml", '[workspace]\nmembers = ["crates/*"]\n')
        put("crates/srv/Cargo.toml", '[package]\nname = "srv"\n[[bin]]\nname = "postgres"\npath = "src/main.rs"\n'
            '[dependencies]\nlib1 = { path = "../lib1" }\n')
        put("crates/srv/src/main.rs", "fn main() { lib1::used(); }\n")
        put("crates/lib1/Cargo.toml", '[package]\nname = "lib1"\n')
        put("crates/lib1/src/lib.rs", "pub fn used() {}\npub fn only_tests_here() {}\n#[cfg(test)]\nmod t {\n    fn x() { super::only_tests_here(); }\n}\n")
        put(unreached_mod.LIST, "# selftest\ncrates/lib1/src/lib.rs\tonly_tests_here\tcfg(test) only\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-qm", "unreached row"], check=True)
        rc_, o_ = cbase()
        assert rc_ == 0 and "1 UNREACHED row(s) proven" in o_, ("a row only test code reaches is proven", o_)
        put(unreached_mod.LIST, "# selftest\ncrates/lib1/src/lib.rs\tonly_tests_here\tx\ncrates/lib1/src/lib.rs\tused\ty\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "a reached row"], check=True)
        rc_, o_ = cbase()
        assert rc_ == 1 and "FAIL UNREACHED crates/lib1/src/lib.rs used: reached" in o_ and "main.rs" in o_, ("a reached row fails", o_)
        put(unreached_mod.LIST, "# selftest\ncrates/lib1/src/lib.rs\tonly_tests_here\tx\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "back to the proven row"], check=True)
        # FILE rows (memacct 2026-10-16T05:0xZ): a module file only tests name is UNREACHED whole, re-proven on every lint run;
        # one non-test path into it (fx2: live() names fx2::load) keeps its findings debt (the born-red case) and base fails
        lib_txt = open(os.path.join(r, "crates/lib1/src/lib.rs")).read()
        put("crates/lib1/src/lib.rs", lib_txt + "pub mod fx;\npub mod fx2;\npub fn live() { fx2::load(); }\n")
        put("crates/lib1/src/fx.rs", "pub fn load() {\n    let v = vec![1];\n}\n")
        put("crates/lib1/src/fx2.rs", "pub fn load() {\n    let v = vec![1];\n}\n")
        put("crates/lib1/tests/a.rs", "fn t() { lib1::fx::load(); lib1::fx2::load(); }\n")
        fp_ = unreached_mod.FILE_PROOF
        put(unreached_mod.LIST, f"# selftest\ncrates/lib1/src/lib.rs\tonly_tests_here\tx\ncrates/lib1/src/fx.rs\t*\t{fp_}: tests only\n"
                                f"crates/lib1/src/fx2.rs\t*\t{fp_}: tests only\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-qm", "file rows"], check=True)
        buf = _io.StringIO()
        with _cl.redirect_stdout(buf):
            sp_ = unreached_spans(r)
        o_ = buf.getvalue()
        assert "crates/lib1/src/fx.rs" in sp_ and "crates/lib1/src/fx2.rs" not in sp_, ("only the proven file is UNREACHED", sp_, o_)
        assert "UNREACHED-FILE NOT PROVEN crates/lib1/src/fx2.rs" in o_ and "live" in o_ and "UNREACHED-FILE crates/lib1/src/fx.rs: proven" in o_, o_
        rc_, o_ = cbase()
        assert rc_ == 1 and "FAIL UNREACHED crates/lib1/src/fx2.rs *: reached" in o_, ("a reached FILE row fails base", o_)
        put(unreached_mod.LIST, f"# selftest\ncrates/lib1/src/lib.rs\tonly_tests_here\tx\ncrates/lib1/src/fx.rs\t*\tno version\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "a file row with no proof version"], check=True)
        rc_, o_ = cbase()
        assert rc_ == 1 and "does not name" in o_, ("a FILE row's note names the proof version", o_)
        put(unreached_mod.LIST, f"# selftest\ncrates/lib1/src/lib.rs\tonly_tests_here\tx\ncrates/lib1/src/fx.rs\t*\t{fp_}: tests only\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "the proven file row"], check=True)
        rc_, o_ = cbase()
        assert rc_ == 0 and "2 UNREACHED row(s) proven" in o_, ("a proven FILE row passes base", o_)
        put(unreached_mod.LIST, "# selftest\ncrates/lib1/src/lib.rs\tonly_tests_here\tx\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "back to the proven row"], check=True)
        # a USER-APPROVED ADD: a trailer citing the user's decision lets that row be added, by at most its count
        open(p, "w").write(f"path\t{f}\ngrowth\tstd::vec::Vec::push\t1\nmacro\tstd::vec\t2\n")
        ok_t = f"Alloclint-Approved: {f} macro std::vec {{}} USER-DECISIONS-20261007.md@2026-10-15T14:56:41Z"
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "approved adds\n\n" + ok_t.format(1)], check=True)
        assert cmd_base(argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD")) == 1, "an add past the approved count fails"
        subprocess.run(["git", "-C", r] + env + ["commit", "-q", "--amend", "-m", "approved adds\n\n" + ok_t.format(2)], check=True)
        assert cmd_base(argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD")) == 0, "an add the user approved passes"
        subprocess.run(["git", "-C", r] + env + ["commit", "-q", "--amend", "-m", f"approved adds\n\nAlloclint-Approved: {f} macro std::vec 2 ok"],
                       check=True)
        assert cmd_base(argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD")) == 1, "an approval that cites no user decision fails"
        # SPACED ITEMS (memacct 2026-10-16 ~23:3xZ): an IR item with spaces round-trips trailer -> allowance -> the ledger row
        # (tab-separated, its spaces intact); a 6-word trailer parses as before; fewer than 5 words after the key is malformed
        cite_ = "USER-DECISIONS-20261007.md@2026-10-16T23:30:19Z"
        sp_item = "<alloc::sync::Arc<[T]> as core::convert::From<[T; N]>>::from"
        assert approved_fields(f"Alloclint-Approved: {f} ir-only {sp_item} 3 {cite_}") == (f, "ir-only", sp_item, 3, cite_)
        assert approved_fields(f"Alloclint-Approved: {f} macro std::vec 2 {cite_}") == (f, "macro", "std::vec", 2, cite_)
        assert approved_fields(f"Alloclint-Approved: {f} macro 2 {cite_}") is None, "4 words after the key: malformed"
        assert approved_fields(f"Alloclint-Approved: {f} ir-only {sp_item} three {cite_}") is None, "a count that is no number"
        assert approved_fields(f"Alloclint-Approved:  {f}  ir-only  {sp_item}  3  {cite_} ") == (f, "ir-only", sp_item, 3, cite_)
        open(p, "w").write(f"path\t{f}\ngrowth\tstd::vec::Vec::push\t1\nir-only\t{sp_item}\t3\n")
        sp_t = f"Alloclint-Approved: {f} ir-only {sp_item} {{}} {cite_}"
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "approved spaced add\n\n" + sp_t.format(3)], check=True)
        buf = _io.StringIO()
        with _cl.redirect_stdout(buf):
            rc_ = cmd_base(argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD"))
        o_ = buf.getvalue()
        assert rc_ == 0 and f"alloclint APPROVED {f} ir-only {sp_item}: up to +3" in o_ and "FAIL" not in o_, ("a spaced item approved", o_)
        subprocess.run(["git", "-C", r] + env + ["commit", "-q", "--amend", "-m", "approved spaced add\n\n" + sp_t.format(2)], check=True)
        buf = _io.StringIO()
        with _cl.redirect_stdout(buf):
            rc_ = cmd_base(argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD"))
        assert rc_ == 1 and f"FAIL ADDS {f} ir-only {sp_item} = 3" in buf.getvalue(), ("past the spaced count", buf.getvalue())
        subprocess.run(["git", "-C", r] + env + ["commit", "-q", "--amend", "-m",
                        "approved spaced add\n\n" + sp_t.format(3).replace("ir-only <alloc", "ir-only  <alloc")], check=True)
        assert cmd_base(argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD")) == 0, "extra blanks between words read as one"
        subprocess.run(["git", "-C", r] + env + ["commit", "-q", "--amend", "-m",
                        "approved spaced add\n\n" + sp_t.format(3).replace(" as core::convert::From<[T; N]>>::from", "")], check=True)
        assert cmd_base(argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD")) == 1, "a truncated item is another key: ADDS"
        subprocess.run(["git", "-C", r] + env + ["commit", "-q", "--amend", "-m",
                        f"approved spaced add\n\nAlloclint-Approved: {f} ir-only 3 {cite_}"], check=True)
        buf = _io.StringIO()
        with _cl.redirect_stdout(buf):
            rc_ = cmd_base(argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD"))
        assert rc_ == 1 and "FAIL APPROVED: malformed trailer" in buf.getvalue(), ("fewer than 5 words after the key", buf.getvalue())
        open(p, "w").write(f"path\t{f}\ngrowth\tstd::vec::Vec::push\t1\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "back again"], check=True)
        # a RENAME (main 2026-10-14T22:10:42Z): the same site's item changes (.clone() -> .to_vec()), its row in renames.tsv
        src = os.path.join(r, f)
        os.makedirs(os.path.dirname(src), exist_ok=True)
        open(src, "w").write("fn a() {}\nlet x = b.params.clone();\nlet y = c.clone();\n")
        open(p, "w").write(f"path\t{f}\ngrowth\tstd::vec::Vec::push\t1\nmethod\tstd::clone::Clone::clone\t2\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-qm", "clone sites"], check=True)
        open(src, "w").write("fn a() {}\n// moved\nlet x = b.params.to_vec();\nlet y = c.clone();\n")
        open(p, "w").write(f"path\t{f}\ngrowth\tstd::vec::Vec::push\t1\nmethod\tslice::to_vec\t1\nmethod\tstd::clone::Clone::clone\t1\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "to_vec"], check=True)
        RB = argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD")
        assert cmd_base(RB) == 1, "an item change with no rename row fails ADDS"
        rp = os.path.join(r, RENAMES)
        hdr = "file\tline\told_kind\told_item\tnew_kind\tnew_item\tcount\tlane\treason\n"
        def ren(line, cnt=1, back=2):
            open(rp, "w").write(hdr + f"{f}\t{line}\tmethod\tstd::clone::Clone::clone\tmethod\tslice::to_vec\t{cnt}\tplanir\tIR lists are not Clone\n")
            subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
            subprocess.run(["git", "-C", r] + env + ["commit", "-qm", "rename row"], check=True)
            return argparse.Namespace(repo=r, base=f"HEAD~{back}", tree="HEAD")
        assert cmd_base(ren(3)) == 0, "a same-site rename (line 3, paired with base line 2 naming clone) passes"
        subprocess.run(["git", "-C", r] + env + ["reset", "-q", "--hard", "HEAD~1"], check=True)
        assert cmd_base(ren(2)) == 1, "a rename row on an ADDED line (a new site) fails"
        subprocess.run(["git", "-C", r] + env + ["reset", "-q", "--hard", "HEAD~1"], check=True)
        assert cmd_base(ren(4)) == 1, "a rename row on an unchanged line fails"
        subprocess.run(["git", "-C", r] + env + ["reset", "-q", "--hard", "HEAD~1"], check=True)
        assert cmd_base(ren(3, 2)) == 1, "a rename that needs the old item to fall by 2 (it falls by 1) fails"
        subprocess.run(["git", "-C", r] + env + ["reset", "-q", "--hard", "HEAD~1"], check=True)
        open(p, "w").write(f"path\t{f}\ngrowth\tstd::vec::Vec::push\t1\nmethod\tslice::to_vec\t2\nmethod\tstd::clone::Clone::clone\t1\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "to_vec 2"], check=True)
        assert cmd_base(ren(3, 2, 3)) == 1, "a rename that raises the file's debt fails"
        subprocess.run(["git", "-C", r] + env + ["reset", "-q", "--hard", "HEAD~4"], check=True)
        # the TYPE-MENTION CATCH-UP (main 2026-10-14T23:41:27Z ruling 1): the range that brings TYPE-MENTION in, named by the
        # trailer, may raise a file's ir-only rows while the file's type + ir-only total does not rise; nothing else rises
        def led(text, msg, amend=False):
            open(p, "w").write(f"path\t{f}\n" + text)
            subprocess.run(["git", "-C", r] + env + ["commit", "-qa"] + (["--amend"] if amend else []) + ["-m", msg], check=True)
        def base_out():
            o = io.StringIO()
            with contextlib.redirect_stdout(o):
                rc_ = cmd_base(argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD"))
            return rc_, o.getvalue()
        TM = "\n\nAlloclint-Catchup: type-mention"
        led("growth\tstd::vec::Vec::push\t1\ntype\tstd::vec::Vec\t3\nir-only\talloc::x\t1\n", "type rows")
        led("growth\tstd::vec::Vec::push\t1\ntype\tstd::vec::Vec\t1\nir-only\talloc::x\t2\nir-only\talloc::y\t1\n", "rewrite" + TM)
        rc, o = base_out()
        assert rc == 0 and "CATCH-UP " + f + " ir-only alloc::x: 1 -> 2" in o and "CATCH-UP " + f + " ir-only alloc::y: 0 -> 1" in o \
            and "TYPE-MENTION catch-up: 2 ir-only site(s)" in o, "a named catch-up within the file's type + ir-only passes, shown: " + o
        led("growth\tstd::vec::Vec::push\t1\ntype\tstd::vec::Vec\t1\nir-only\talloc::x\t2\nir-only\talloc::y\t1\n", "rewrite", True)
        assert base_out()[0] == 1, "an ir-only raise with no trailer fails"
        led("growth\tstd::vec::Vec::push\t1\ntype\tstd::vec::Vec\t1\nir-only\talloc::x\t3\nir-only\talloc::y\t1\n", "rewrite" + TM, True)
        rc, o = base_out()
        assert rc == 1 and o.count("FAIL CATCH-UP " + f + ": its type + ir-only total rises 4 -> 5") == 1, "a file whose total rises fails once: " + o
        led("growth\tstd::vec::Vec::push\t2\ntype\tstd::vec::Vec\t1\nir-only\talloc::x\t1\n", "rewrite" + TM, True)
        assert base_out()[0] == 1, "the trailer lets only ir-only rows rise (a growth raise fails)"
        fp = os.path.join(r, "crates/gravity/lint/alloclint/findings.py")
        os.makedirs(os.path.dirname(fp), exist_ok=True)
        open(fp, "w").write("def is_mention(x, lines):\n    pass\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        led("growth\tstd::vec::Vec::push\t1\ntype\tstd::vec::Vec\t3\nir-only\talloc::x\t1\n", "type rows, TYPE-MENTION in")
        led("growth\tstd::vec::Vec::push\t1\ntype\tstd::vec::Vec\t1\nir-only\talloc::x\t2\n", "again" + TM)
        assert base_out()[0] == 1, "one range only: a base that already has TYPE-MENTION takes no catch-up"
        subprocess.run(["git", "-C", r] + env + ["reset", "-q", "--hard", "HEAD~4"], check=True)
        # D4505 CHARGED-CALLER SEED (USER GO via main 2026-10-15T05:52:10Z): the range that brings the rule in (its base's
        # homes.py has no caller_lines), named by the trailer, may raise a file's rows by at most its unlisted `charged caller`
        # lines; with no trailer, past that count, in a file with none, or on a later base, a rise fails as before
        os.makedirs(os.path.dirname(os.path.join(r, f)), exist_ok=True)
        open(os.path.join(r, f), "w").write(CC_SRC)
        os.makedirs(os.path.dirname(os.path.join(r, homes_mod.CLIPPY)), exist_ok=True)
        open(os.path.join(r, homes_mod.CLIPPY), "w").write(CC_TOML)
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        CC = "\n\nAlloclint-Seed: charged-caller"
        led("growth\tstd::vec::Vec::push\t1\n", "writers")
        led("growth\tstd::vec::Vec::push\t2\nmethod\tstd::vec::Vec::with_capacity\t1\n", "seed" + CC)
        rc, o = base_out()
        assert rc == 0 and "SEED charged-caller " + f + " growth std::vec::Vec::push: 1 -> 2" in o \
            and "SEED charged-caller " + f + " method std::vec::Vec::with_capacity: 0 -> 1" in o \
            and " sites + the 2 charged-caller seed rows (D4505, 1 file(s)))" in o, "a named seed within the lines passes: " + o
        led("growth\tstd::vec::Vec::push\t3\n", "seed", True)
        rc, o = base_out()
        assert rc == 1 and "FAIL RAISES " + f + " growth std::vec::Vec::push: 1 -> 3" in o, "no trailer: the raise fails: " + o
        led("growth\tstd::vec::Vec::push\t5\n", "seed" + CC, True)
        rc, o = base_out()
        assert rc == 0 and "room left in the file 0" in o, "a rise of 2 x the file's 2 lines passes: " + o
        led("growth\tstd::vec::Vec::push\t6\n", "seed" + CC, True)
        rc, o = base_out()
        assert rc == 1 and "FAIL RAISES " + f + " growth std::vec::Vec::push: 1 -> 6" in o, "a rise past 2 x the lines fails: " + o
        open(os.path.join(r, f), "a").write("fn more(out: &mut Vec<u8>) {\n    out.push(3);\n}\n")
        led("growth\tstd::vec::Vec::push\t2\n", "seed" + CC, True)
        rc, o = base_out()
        assert rc == 1 and "FAIL RAISES " + f in o, "a range that changes the file's source takes no seed: " + o
        open(os.path.join(r, f), "w").write(CC_SRC)
        open(os.path.join(r, homes_mod.CLIPPY), "w").write(CC_TOML.replace("]\n", '  { path = "lowerv4::a::enc", reason = "alloclint:growth" },\n]\n'))
        led("growth\tstd::vec::Vec::push\t2\n", "seed" + CC, True)
        rc, o = base_out()
        assert rc == 1 and "FAIL RAISES " + f in o, "every caller line in a listed writer: no seed room: " + o
        open(os.path.join(r, homes_mod.CLIPPY), "w").write(CC_TOML)
        hp3 = os.path.join(r, "crates/gravity/lint/alloclint/homes.py")
        open(hp3, "w").write("def caller_lines(path, text, listed):\n    pass\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        led("growth\tstd::vec::Vec::push\t1\n", "the rule in", True)
        led("growth\tstd::vec::Vec::push\t2\n", "again" + CC)
        rc, o = base_out()
        assert rc == 1 and "FAIL RAISES " + f in o, "one range only: a base that already has the rule takes no seed: " + o
        subprocess.run(["git", "-C", r] + env + ["reset", "-q", "--hard", "HEAD~3"], check=True)
        for x in (os.path.join(r, f), os.path.join(r, homes_mod.CLIPPY), hp3):
            if os.path.exists(x):
                os.remove(x)
        # MCX-TRANSMUTE (USER APPROVED via main 2026-10-15T00:10:23Z): the seed range (trailer, a full scan equals the rows), then
        # every changed file's sites equal its rows: a new site FAILs NEW, a removed one STALE, a raise RAISES, a home is tracked
        start = subprocess.run(["git", "-C", r, "rev-parse", "HEAD"], capture_output=True, text=True, check=True).stdout.strip()
        bf = "crates/backend/x/src/lib.rs"
        bsrc = os.path.join(r, bf)
        os.makedirs(os.path.dirname(bsrc), exist_ok=True)
        T1 = "fn a(cx: &MemoryContext) {\n    let m: Mcx<'static> = unsafe { core::mem::transmute(cx.mcx()) };\n}\n"
        open(bsrc, "w").write(T1)
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-qm", "a transmute"], check=True)
        md = os.path.join(r, LEDGER, mcxt.SUBDIR)
        os.makedirs(md)
        mp = os.path.join(md, mcxt.ledger_name(bf))
        def mrow(n, msg, amend=False):
            open(mp, "w").write(f"path\t{bf}\n{mcxt.KIND}\ttransmute\t{n}\n")
            subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
            subprocess.run(["git", "-C", r] + env + ["commit", "-q"] + (["--amend"] if amend else []) + ["-m", msg], check=True)
        SEED = "\n\nAlloclint-Seed: mcx-transmute"
        mrow(1, "seed" + SEED)
        rc, o = base_out()
        assert rc == 0 and "mcx-transmute: 1 site(s) seeded by trailer" in o, "the named seed equal to a full scan passes: " + o
        assert " + the 1 mcx-transmute seed rows)" in o and "->" in o, "the seed's rows are counted apart from the old rows: " + o
        mrow(1, "seed", True)
        rc, o = base_out()
        assert rc == 1 and "FAIL MCX SEED" in o, "a seed with no trailer fails: " + o
        mrow(2, "seed" + SEED, True)
        rc, o = base_out()
        assert rc == 1 and "FAIL MCX STALE " + bf in o, "a seed above the census fails (the full scan): " + o
        mrow(1, "seed" + SEED, True)
        open(bsrc, "w").write(T1 + "fn b(cx: &MemoryContext) {\n    let n = unsafe { core::mem::transmute::<Mcx<'_>, Mcx<'static>>(cx.mcx()) };\n}\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "a new site"], check=True)
        rc, o = base_out()
        assert rc == 1 and "FAIL MCX NEW " + bf + ":2,5 mcx-transmute transmute: 2 site(s), budget 1" in o, "a new site is refused: " + o
        mrow(2, "a new site, its row raised", True)
        rc, o = base_out()
        assert rc == 1 and "FAIL RAISES " + bf + " mcx-transmute transmute: 1 -> 2" in o, "a raised row fails: " + o
        mrow(1, "a new site in a home", True)
        hp2 = os.path.join(r, HOMES)
        open(hp2, "a").write(f"{bf}\tb\tH3\tPinnedContext::handle\n")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-q", "--amend", "-m", f"pinned\n\nAlloclint-Home: {bf} b"], check=True)
        rc, o = base_out()
        assert rc == 0 and "mcx-transmute: " in o, "a site in a named home is tracked: " + o
        subprocess.run(["git", "-C", r] + env + ["reset", "-q", "--hard", "HEAD~1"], check=True)
        open(bsrc, "w").write("fn a(cx: &MemoryContext) {}\n")
        subprocess.run(["git", "-C", r] + env + ["commit", "-qam", "the site removed"], check=True)
        rc, o = base_out()
        assert rc == 1 and "FAIL MCX STALE " + bf + " mcx-transmute transmute: budget 1, now 0" in o, "a removed site leaves a stale row: " + o
        subprocess.run(["git", "-C", r] + env + ["rm", "-q", mp], check=True)
        os.makedirs(md, exist_ok=True)
        open(os.path.join(md, ".empty"), "w").write("")
        subprocess.run(["git", "-C", r] + env + ["add", "-A"], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-q", "--amend", "-m", "the site removed, its row lowered"], check=True)
        rc, o = base_out()
        assert rc == 0, "lowered with the code passes: " + o
        subprocess.run(["git", "-C", r] + env + ["reset", "-q", "--hard", start], check=True)
        subprocess.run(["git", "-C", r] + env + ["rm", "-rq", LEDGER], check=True)
        subprocess.run(["git", "-C", r] + env + ["commit", "-qm", "drop"], check=True)
        assert cmd_base(argparse.Namespace(repo=r, base="HEAD~1", tree="HEAD")) == 1, "deleting the ledger fails"
    import contextlib, io
    # .noalloc rows of a REPEATED line text survive a moved file (alloclint 2026-10-16: two rows whose nearest same-text line was
    # one line overwrote each other in a --lower-only write; the lost row's finding read as debt, FAIL NEW at the next check)
    with tempfile.TemporaryDirectory() as t:
        f = CRATE + "/src/r.rs"
        src = os.path.join(t, f)
        os.makedirs(os.path.dirname(src), exist_ok=True)
        line = "    let a = k.clone();"
        body = ["fn f() {"] + [line if i in (2, 10) else "    let z%d = 0;" % i for i in range(2, 12)] + ["}"]
        open(src, "w").write("\n".join(body) + "\n")

        def cl(ls):
            return "".join(json.dumps({"reason": "compiler-message", "message": {
                "code": {"code": "clippy::disallowed_methods"}, "level": "warning", "message": "use of a disallowed method `std::clone::Clone::clone`",
                "spans": [{"is_primary": True, "file_name": f, "line_start": l_, "column_start": 13}],
                "children": [{"message": "alloclint:method"}]}}) + "\n" for l_ in ls)
        js = os.path.join(t, "c.json")
        open(js, "w").write(cl([2, 10]))
        nat = os.path.join(t, "noalloc.tsv")
        tk = findings_mod.line_key(line)
        open(nat, "w").write("file\tline\tkind\titem\tcount\ttext\n" + "".join(
            f"{f}\t{l_}\tmethod\tstd::clone::Clone::clone\t1\t{tk}\n" for l_ in (2, 10)))
        assert cmd_write(argparse.Namespace(findings=js, ir_misses=None, root=t, crate=[CRATE], noalloc=nat)) == 0
        nap = os.path.join(t, LEDGER, crate_key(CRATE), NOALLOC_FILE)
        assert sum(1 for x in open(nap) if x.startswith(f)) == 2, open(nap).read()
        open(src, "w").write("\n".join(["// moved"] * 10 + body) + "\n")   # both lines move down 10: 12 and 20
        open(js, "w").write(cl([12, 20]))
        assert cmd_write(argparse.Namespace(findings=js, ir_misses=None, root=t, crate=[CRATE], noalloc=None, lower_only=True)) == 0
        rows = sorted(int(x.split("\t")[1]) for x in open(nap) if x.startswith(f))
        assert rows == [12, 20], ("each row keeps its own line", rows, open(nap).read())
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            rc_ = cmd_check(argparse.Namespace(findings=js, ir_misses=None, root=t, crate=[CRATE]))
        assert rc_ == 0 and "NOALLOC 2 " in buf.getvalue(), ("both moved clones stay NOALLOC", buf.getvalue())
    print("ratchet selftest: ok")
    return 0


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("check", "write"):
        p = sub.add_parser(name)
        p.add_argument("--findings", required=True)
        p.add_argument("--ir-misses")
        p.add_argument("--noalloc", help="the IR leg's x/noalloc.tsv (run.sh full): the NOALLOC findings it proves on this tree")
        p.add_argument("--root", default=".")
        p.add_argument("--crate", action="append", default=[], help="a crate dir of scope.tsv (repeatable; default: every one)")
        if name == "write":
            p.add_argument("--file", action="append", default=[],
                           help="write: rewrite ONLY this source file's ledger file (repeatable; the crate's other files untouched)")
            p.add_argument("--lower-only", action="store_true",
                           help="write: every row min(budget, now); no row added, no budget raised (the NEW rows stay NEW)")
    p = sub.add_parser("base")
    p.add_argument("--base", required=True)
    p.add_argument("--tree")
    p.add_argument("--repo", default=".")
    sub.add_parser("selftest")
    a = ap.parse_args()
    return {"check": cmd_check, "write": cmd_write, "base": cmd_base, "selftest": cmd_selftest}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
