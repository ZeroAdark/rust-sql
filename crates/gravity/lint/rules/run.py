#!/usr/bin/env python3
"""The v5 rules runner (PLAN-v5 §3.2-§3.3), run over the checkout on every landing. Not a compiled test binary: it reads
the tree it is pointed at, as text (D793: a reused lint binary scanned another tree and passed).

    run.py [--root DIR] [--base REV] [--open-bugs FILE] [--tsv OUT] [--write-legacy] [--quiet]
    run.py --selftest

Full check: every rule over every production source file; a hit fails unless its file's directory is a line of
crates/gravity/lint/LEGACY (one directory per line, its own files only, not its subdirectories). With --base (the
parent): every added line anywhere is checked too: a landing may move a hit unchanged or edit a line that already hit,
but may not add a hit, in LEGACY directories included; and LEGACY may gain a line only for a directory that received
hit lines moved unchanged. The runner fails when it scans zero files, when a LEGACY line names a directory that does not
exist or holds no source file, and when a LEGACY directory passes every rule (delete the line). An ON_TOUCH rule (RP)
never fails the full check, and on a landing fails every added or moved line it hits (strip on touch; notes instead while
its WARN file exists). Exit 0 = green.
"""
import argparse
import collections
import importlib
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import common  # noqa: E402
import exempt  # noqa: E402
from common import G, Src, in_any, normalize_line, read, walk_sources  # noqa: E402

LEGACY = G + "lint/LEGACY"
EXEMPT = G + "lint/rules/exempt.py"
_ALLOW_INNER = re.compile(r"^\s*#!\[\s*allow\(\s*unsafe_code\s*\)\s*\]\s*$")
ORDER = ["R1", "R2", "R3", "R4", "R5", "R6", "R7", "R8", "R9", "R10", "R11", "R12", "R13", "R14", "RA", "RB", "RV", "RL", "RK", "RM", "RS", "A1", "C4", "C6", "C8",
         "C14", "C29", "C30", "C32", "RP"]


def load_rules():
    mods = []
    for f in sorted(os.listdir(HERE)):
        if f.endswith(".py") and f not in ("common.py", "run.py", "attrs.py", "exempt.py"):
            mods.append(importlib.import_module(f[:-3]))
    mods.sort(key=lambda m: ORDER.index(m.ID) if m.ID in ORDER else len(ORDER))
    return mods


def id_clashes(rules):
    """-> one "<ID>: <rule files>" per id that more than one rule declares. A FAIL line ([ID]), the per-rule counters, an
    exempt row and a lane's STATUS name a rule by its id alone, so two rules on one id read as one (the column-demand
    and key-image rules once shared RD and RI with two others); the runner refuses to run such a rule set."""
    by = collections.defaultdict(list)
    for r in rules:
        by[r.ID].append(os.path.basename(getattr(r, "__file__", "?")))
    return [f"{i}: {', '.join(sorted(fs))}" for i, fs in sorted(by.items()) if len(fs) > 1]


def on_touch(rule):
    """An ON_TOUCH rule (RP) fails only lines a landing adds or moves: the tree's hits are legacy wherever they are, and
    they keep no LEGACY line alive."""
    return getattr(rule, "ON_TOUCH", False)


def is_home(rule, path):
    f = getattr(rule, "is_home", None)
    return f(path) if f else in_any(path, getattr(rule, "HOME", []))


def file_hits(rule, src, ctx):
    """-> [line] for one rule over one file (scope, home and the exempt table applied)."""
    if getattr(rule, "GLOBAL", False) or not rule.SCOPE(src.path):
        return []
    if is_home(rule, src.path) and rule.ID != "R6":        # R6 also checks its homes' crate roots for the deny line
        return []
    needs = getattr(rule, "NEEDS", None)
    if needs and not any(n in src.text for n in needs):
        return []
    hits = sorted(set(rule.check(src, ctx))) if hasattr(rule, "check") else src.hits(rule.PATTERN)
    return exempt.excused(rule.ID, src, hits, ctx.exempt_used)


def row_hits(rule, src, ctx):
    """`file_hits`; a GLOBAL rule that charges its sites per file (`site_lines`, RL2) takes the exempt table too."""
    if getattr(rule, "GLOBAL", False) and hasattr(rule, "site_lines"):
        return exempt.excused(rule.ID, src, rule.site_lines(src), ctx.exempt_used)
    return file_hits(rule, src, ctx)


class Ctx:
    def __init__(self, root, open_bugs=None):
        self.root, self.open_bugs, self.cache, self.notes = root, open_bugs, {}, []
        self.exempt_used = set()           # the exempt rows that excused a hit of the tree
        self.base_read, self.base_manifests, self.base_ledger = None, [], None

    def read(self, path):
        try:
            return read(self.root, path)
        except OSError:
            return None

    def ledger(self, path):
        """A debt ledger directory (one row per file, common.ledger_join) -> (text, at); a ledger still in one file reads
        as itself; (None, None) when absent."""
        d = os.path.join(self.root, path)
        if os.path.isdir(d):
            entries = []
            for name in os.listdir(d):
                if os.path.isfile(os.path.join(d, name)):
                    entries.append((name, read(self.root, path + "/" + name)))
            return common.ledger_join(path, entries)
        t = self.read(path)
        return common.ledger_file(path, t) if t is not None else (None, None)


# Every call names its repository by `-C root`. A git hook exports GIT_DIR (in a linked worktree), GIT_INDEX_FILE and maybe
# GIT_WORK_TREE; inherited, they point the call at the hook's repository instead: the selftest's scratch `git init` then
# re-initialized the shared repository as bare (core.bare=true for every worktree of it).
GIT_ENV = {k: v for k, v in os.environ.items() if k not in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_PREFIX")}


def git(root, *args, inp=None):
    r = subprocess.run(["git", "-C", root] + list(args), input=inp, capture_output=True, env=GIT_ENV)
    if r.returncode != 0:
        raise SystemExit(f"rules: git {' '.join(args)} failed: {r.stderr.decode(errors='replace').strip()}")
    return r.stdout


def base_reader(root, rev):
    cache = {}

    def rd(path):
        if path not in cache:
            r = subprocess.run(["git", "-C", root, "show", f"{rev}:{path}"], capture_output=True, env=GIT_ENV)
            cache[path] = r.stdout.decode("utf-8", "replace") if r.returncode == 0 else None
        return cache[path]
    return rd


def parse_diff(text):
    """-> {path: {"add": [(line, text)], "del": [(line, text)], "base": path or None}} from `git diff -U0 --no-renames`."""
    out, cur, a, b, pending_old, header = {}, None, 0, 0, None, False
    for line in text.split("\n"):
        if line.startswith("diff --git "):
            cur, header = None, True
        elif header and line.startswith("--- "):
            old = line[4:]
            pending_old = None if old == "/dev/null" else old[2:]
        elif header and line.startswith("+++ "):
            new = line[4:]
            path = pending_old if new == "/dev/null" else new[2:]
            cur = out.setdefault(path, {"add": [], "del": [], "base": pending_old})
            header = False
        elif line.startswith("@@"):
            m = re.match(r"@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@", line)
            a, b = int(m.group(1)), int(m.group(2))
        elif cur is not None and line.startswith("-"):
            cur["del"].append((a, line[1:]))
            a += 1
        elif cur is not None and line.startswith("+"):
            cur["add"].append((b, line[1:]))
            b += 1
    return out


def load_open_bugs(path):
    """Open D-ids: one per line, or BUGS-OPEN.tsv (a row whose last column, status_under_rulings, starts with
    closed / moot is not open)."""
    ids = set()
    for line in open(path, encoding="utf-8", errors="replace"):
        m = re.match(r"\s*(D\d{3,4})\b", line)
        if m and not re.match(r"\s*(?:closed|moot)\b", line.rstrip("\n").split("\t")[-1]):
            ids.add(m.group(1))
    return ids


def read_legacy(root):
    p = os.path.join(root, LEGACY)
    if not os.path.isfile(p):
        return [], None
    lines = []
    for n, raw in enumerate(open(p, encoding="utf-8"), 1):
        s = raw.split("#", 1)[0].strip().rstrip("/")
        if s:
            lines.append((n, s))
    return lines, p


def base_ledger_reader(root, rev):
    """The parent's debt ledger -> its rows' text (a directory joined as Ctx.ledger joins it, a single file as is), None
    when the parent has none: the "the list only shrinks" checks read the parent through it."""
    def rd(path):
        r = subprocess.run(["git", "-C", root, "ls-tree", rev, "--", path], capture_output=True, env=GIT_ENV)
        top = r.stdout.decode("utf-8", "replace").strip()
        if r.returncode or not top:
            return None
        if " blob " in top.split("\t")[0]:
            return base_reader(root, rev)(path)
        ls = subprocess.run(["git", "-C", root, "ls-tree", f"{rev}:{path}"], capture_output=True, env=GIT_ENV)
        rows = [x.split("\t", 1) for x in ls.stdout.decode("utf-8", "replace").split("\n") if x.strip()]
        blobs = [(name, meta.split()[2]) for meta, name in rows if meta.split()[1] == "blob"]
        cat = subprocess.run(["git", "-C", root, "cat-file", "--batch"], input="".join(f"{sha}\n" for _, sha in blobs).encode(),
                             capture_output=True, env=GIT_ENV)
        out, i, entries = cat.stdout, 0, []
        for name, _ in blobs:
            j = out.index(b"\n", i)
            size = int(out[i:j].split()[2])
            entries.append((name, out[j + 1:j + 1 + size].decode("utf-8", "replace")))
            i = j + 1 + size + 1
        return common.ledger_join(path, entries)[0]
    return rd


def run(root, base=None, open_bugs=None, tsv=None, write_legacy=False, quiet=False, out=sys.stdout):
    t0 = time.time()
    rules = load_rules()
    clash = id_clashes(rules)
    if clash:
        for c in clash:
            print(f"rules: FAIL: two rules share an id ({c}): give the later one its own", file=out)
        return 1
    ctx = Ctx(root, open_bugs)
    paths = walk_sources(root)
    srcs = []
    for p in paths:
        t = read(root, p)
        if common.generated(t):
            continue
        srcs.append(Src(p, t))
    fails, legacy_hits, all_hits = [], collections.Counter(), []
    if not srcs:
        print("rules: FAIL: scanned zero source files (wrong --root?)", file=out)
        return 1
    legacy_lines, _ = read_legacy(root)
    legacy = {d for _, d in legacy_lines}
    by_dir = collections.defaultdict(list)
    for s in srcs:
        by_dir[os.path.dirname(s.path)].append(s)
    per_rule = collections.defaultdict(lambda: [0, 0, set()])
    hits_by_file = {}
    for s in srcs:
        d = os.path.dirname(s.path)
        for r in rules:
            ls = file_hits(r, s, ctx)
            if not ls:
                continue
            st = per_rule[r.ID]
            st[2].add(s.path)
            if on_touch(r):
                for ln in ls:
                    line = s.lines[ln - 1].strip() if ln - 1 < len(s.lines) else ""
                    all_hits.append((r.ID, s.path, ln, "legacy", line[:160]))
                st[1] += len(ls)
                continue
            hits_by_file[(r.ID, s.path)] = ls
            for ln in ls:
                line = s.lines[ln - 1].strip() if ln - 1 < len(s.lines) else ""
                all_hits.append((r.ID, s.path, ln, "legacy" if d in legacy else "fail", line[:160]))
                if d in legacy:
                    st[1] += 1
                    legacy_hits[d] += 1
                else:
                    st[0] += 1
                    fails.append(f"{s.path}:{ln}: [{r.ID}] {r.TITLE}; instead: {r.INSTEAD} | {line[:120]}")
        for r in rules:
            if hasattr(r, "check_status"):
                for ln, note in r.check_status(s, ctx):
                    fails.append(f"{s.path}:{ln}: [{r.ID}] {note}")
    for r in rules:
        if getattr(r, "GLOBAL", False):
            if base:
                ctx.base_read = base_reader(root, base)
                ctx.base_ledger = base_ledger_reader(root, base)
                ctx.base_manifests = [p for p in git(root, "ls-tree", "-r", "--name-only", base, "--", G)
                                      .decode().split("\n") if p.endswith("Cargo.toml")]
            for p, n, note in r.check_tree(ctx):
                per_rule[r.ID][0] += 1
                per_rule[r.ID][2].add(p)
                fails.append(f"{p}:{n}: [{r.ID}] {note}")
                all_hits.append((r.ID, p, n, "fail", note))
    # the exempt table checks itself against the tree it ships in: a row that excuses no hit goes
    if os.path.isfile(os.path.join(root, EXEMPT)):
        for row in exempt.ROWS:
            if row[:3] not in ctx.exempt_used:
                fails.append(f"{EXEMPT}: row {row[0]} {row[1]}:{row[2]} excuses no hit: delete the row")
        if not quiet:
            ctx.notes.append(f"exempt: {len(ctx.exempt_used)} of {len(exempt.ROWS)} rows excuse a hit ({EXEMPT})")
    # LEGACY: the runner's own checks
    if write_legacy:
        dirty = sorted({os.path.dirname(p) for (rid, p) in hits_by_file})
        write_legacy_file(root, dirty)
        legacy_lines, _ = read_legacy(root)
        legacy = {d for _, d in legacy_lines}
        return run(root, base, open_bugs, tsv, False, quiet, out)
    seen = set()
    for n, d in legacy_lines:
        if d in seen:
            fails.append(f"{LEGACY}:{n}: duplicate line {d}")
        seen.add(d)
        if not os.path.isdir(os.path.join(root, d)):
            fails.append(f"{LEGACY}:{n}: {d} does not exist (replace the line with the paths its files moved to)")
        elif not by_dir.get(d):
            fails.append(f"{LEGACY}:{n}: {d} holds no source file the rules read")
        elif legacy_hits[d] == 0:
            fails.append(f"{LEGACY}:{n}: {d} passes every rule: delete the line")
    # A landing: no added hit, LEGACY included; LEGACY grows only by moves. The landing that adds the rules (its parent
    # has no LEGACY) is checked by the selftest and the full check alone.
    if base and subprocess.run(["git", "-C", root, "cat-file", "-e", f"{base}:{LEGACY}"], capture_output=True, env=GIT_ENV).returncode:
        ctx.notes.append(f"the parent {base} has no {LEGACY}: the added-line check is skipped (the rules' own landing)")
    elif base:
        fails += diff_check(root, base, rules, ctx, legacy)
    dt = time.time() - t0
    if tsv:
        with open(tsv, "w") as fh:
            fh.write("rule\tpath\tline\tstatus\ttext\n")
            for row in all_hits:
                fh.write("\t".join(str(x).replace("\t", " ") for x in row) + "\n")
    if not quiet:
        print(f"rules: {len(srcs)} files, {len(rules)} rules, {len(legacy)} LEGACY lines, {dt:.2f}s", file=out)
        for r in rules:
            f, l, files = per_rule[r.ID]
            print(f"  {r.ID:4} fail {f:5}  legacy {l:5}  files {len(files):4}  {r.TITLE}", file=out)
        for n in ctx.notes:
            print("  note: " + n, file=out)
    for f in fails[:400]:
        print("FAIL " + f, file=out)
    if len(fails) > 400:
        print(f"FAIL ... and {len(fails) - 400} more", file=out)
    print(f"rules: {'RED' if fails else 'GREEN'} ({len(fails)} failures)", file=out)
    return 1 if fails else 0


def diff_check(root, base, rules, ctx, legacy):
    gpaths = [G] + [common.PLANNER + f for f in common.PLANNER_FILES]
    diff = parse_diff(git(root, "diff", "--no-renames", "-U0", base, "--", *gpaths).decode("utf-8", "replace"))
    # files not yet tracked count as wholly added (the gate runs on a commit, where there are none)
    for p in git(root, "ls-files", "--others", "--exclude-standard", "--", *gpaths).decode().split("\n"):
        if p and p not in diff:
            text = read(root, p)
            diff[p] = {"add": list(enumerate(text.split("\n"), 1)), "del": [], "base": None}
    moved = collections.Counter(normalize_line(t) for f in diff.values() for _, t in f["del"])
    rd = base_reader(root, base)
    fails, moved_in = [], collections.defaultdict(int)
    files, consumed = [], collections.Counter()   # pass 1: which added hit lines are moves (each takes one deleted line)
    for path, ch in sorted(diff.items()):
        if not path.endswith(".rs") or not os.path.isfile(os.path.join(root, path)):
            continue
        if not (common.in_src(path) or common.in_planner(path)):
            continue
        # the generator's output (fn/src/shims/) is skipped here as in the whole-tree scan (`common.rs_files`)
        if common.in_any(path, common.GENERATED_DIRS):
            continue
        tip = Src(path, read(root, path))
        old_text = rd(ch["base"]) if ch["base"] else None
        old = Src(ch["base"], old_text) if old_text is not None else None
        added = {n: t for n, t in ch["add"]}
        removed = {n for n, _ in ch["del"]}
        tip_hits = {r.ID: [n for n in file_hits(r, tip, ctx) if n in added] for r in rules}
        is_moved = {}
        for n in sorted({n for r in rules if not on_touch(r) for n in tip_hits[r.ID]}):   # each added line is matched once
            k = normalize_line(added[n])
            is_moved[n] = moved[k] > 0
            if is_moved[n]:
                moved[k] -= 1
                consumed[k] += 1
                moved_in[os.path.dirname(path)] += 1
        files.append((path, tip, old, added, removed, tip_hits, is_moved))
    old_hits = {}

    def ohits(path, old, removed, r):   # the rule's hits on base lines this landing removes
        if (path, r.ID) not in old_hits:
            old_hits[path, r.ID] = [n for n in file_hits(r, old, ctx) if n in removed] if old else []
        return old_hits[path, r.ID]
    # One deleted line excuses at most one added hit line. A deleted line a move took is not also an old hit (`oh`) that
    # excuses a new hit: it used to count twice (the moved Counter, then `oh`), so one deleted line excused two added ones
    # (D2638, lockrows#408: rules-check GREEN on base bc4599bd4b8, RED on its solo base after #400 landed). Each move takes a
    # deleted line of its text: first one no rule hits (it excuses nothing anyway), then an old hit of a rule its file adds
    # no new hit for, last an old hit a new hit of its file could use.
    spent, elig = set(), {f[0]: f for f in files}
    if consumed:
        dels = collections.defaultdict(list)
        for path, ch in sorted(diff.items()):
            for n, t in ch["del"]:
                k = normalize_line(t)
                if consumed[k] > 0:
                    f = elig.get(path)
                    hit = [r for r in rules if f and not on_touch(r) and n in ohits(path, f[2], f[4], r)]
                    need = any(not f[6][m] for r in hit for m in f[5][r.ID])
                    dels[k].append((bool(hit), need, path, n))
        for k, lst in dels.items():
            spent.update((path, n) for _, _, path, n in sorted(lst)[:consumed[k]])
    for path, tip, old, added, removed, tip_hits, is_moved in files:   # pass 2: the added hits no move or old hit excuses
        for r in rules:
            if on_touch(r):                  # strip on touch: an added or moved line that hits fails, no excuse
                warn = getattr(r, "WARN", None)
                warn = warn and os.path.isfile(os.path.join(root, warn))     # the warn-only window: notes, not failures
                for n in tip_hits[r.ID]:
                    line = f"{path}:{n}: [{r.ID}] touched: {r.TITLE}; instead: {r.INSTEAD} | {added[n].strip()[:120]}"
                    (ctx.notes if warn else fails).append(("warn-only while " + r.WARN + " exists: " if warn else "") + line)
                continue
            new = [n for n in tip_hits[r.ID] if not is_moved[n]]
            if r.ID == "R6" and new:
                # a file-level #![allow(unsafe_code)] added with a split or a new home passes when the file holds unsafe
                # that is not itself added (moved here, or already here): the compiler half requires the line
                held = [n for n in file_hits(r, tip, ctx) if n not in new and not _ALLOW_INNER.match(tip.code[n - 1])]
                if held:
                    new = [n for n in new if not _ALLOW_INNER.match(tip.code[n - 1])]
            if not new:
                continue
            oh = [n for n in ohits(path, old, removed, r) if (path, n) not in spent]
            for n in new[len(oh):]:
                fails.append(f"{path}:{n}: [{r.ID}] added: {r.TITLE}; instead: {r.INSTEAD} | {added[n].strip()[:120]}")
    if LEGACY in diff:
        for n, t in diff[LEGACY]["add"]:
            d = t.split("#", 1)[0].strip().rstrip("/")
            if d and moved_in[d] == 0:
                fails.append(f"{LEGACY}:{n}: {d} added, but no hit line moved into it on this landing "
                             "(a line is added only for the paths a move or split went to)")
    return fails


def write_legacy_file(root, dirs):
    p = os.path.join(root, LEGACY)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, "w") as fh:
        fh.write("# Directories not yet clean (PLAN-v5 §3.3). Each line covers that directory's own files, not its\n"
                 "# subdirectories. A line goes when its directory passes the rules (lint/rules/run.py), or is replaced by\n"
                 "# the paths a certified move or split sent its files to. Nothing else adds a line.\n")
        for d in dirs:
            fh.write(d + "\n")


# ---------------------------------------------------------------------------------------------------------------------
# Self-test: each rule fails its planted line(s) and passes its clean line(s); the runner fails closed.

def selftest():
    rules = load_rules()
    bad = []
    tmp = tempfile.mkdtemp(prefix="v5rules-selftest-")
    try:
        for r in rules:
            if getattr(r, "GLOBAL", False):
                more = [(f"planted{k}", c, True) for k, c in enumerate(getattr(r, "MORE_PLANTED_TOML", ()), 2)]
                more += [(f"clean{k}", c, False) for k, c in enumerate(getattr(r, "MORE_CLEAN_TOML", ()), 2)]
                for name, case, want in [("planted", r.PLANTED_TOML, True), ("clean", r.CLEAN_TOML, False)] + more:
                    root = os.path.join(tmp, f"{r.ID}-{name}")
                    # a case is (path, text), or {"files": [(path, text), ..], "base": {ledger: the parent's rows},
                    # "want": a text one of the hits must hold} (RT: a source file with its registry, a parent's ledger)
                    files = case["files"] if isinstance(case, dict) else [case]
                    for fp, ft in files:
                        _write(root, fp, ft)
                    ctx = Ctx(root)
                    if isinstance(case, dict) and "base" in case:
                        ctx.base_ledger = case["base"].get
                    hits = r.check_tree(ctx)
                    got = bool(hits) if not (isinstance(case, dict) and "want" in case) else \
                        any(case["want"] in h[2] for h in hits)
                    print(f"  {r.ID:4} {name:7} {'hit ' if got else 'pass'}  {files[0][0]}"
                          + (f" ({case['want']})" if isinstance(case, dict) and "want" in case else ""))
                    if got != want:
                        bad.append(f"{r.ID} {name}: expected {'a hit' if want else 'no hit'}")
                continue
            for name, cases, want in (("planted", r.PLANTED, True), ("clean", r.CLEAN, False)):
                for k, (path, text) in enumerate(cases):
                    root = os.path.join(tmp, f"{r.ID}-{name}-{k}")
                    for fp, ft in getattr(r, "FIXTURES", []):
                        _write(root, fp, ft)
                    _write(root, path, text)
                    ctx = Ctx(root)
                    got = bool(file_hits(r, Src(path, text), ctx))
                    print(f"  {r.ID:4} {name:7} {'hit ' if got else 'pass'}  {path} :: {text.strip().splitlines()[-1][:70]}")
                    if got != want:
                        bad.append(f"{r.ID} {name} {path}: expected {'a hit' if want else 'no hit'}")
        for r in rules:                      # the checks LEGACY cannot excuse
            for name, cases, want in (("planted", getattr(r, "PLANTED_STATUS", []), True),
                                      ("clean", getattr(r, "CLEAN_STATUS", []), False)):
                for k, (path, text) in enumerate(cases):
                    ctx = Ctx(tmp, getattr(r, "STATUS_OPEN", None))
                    got = bool(r.check_status(Src(path, text), ctx))
                    print(f"  {r.ID:4} {name:7} {'hit ' if got else 'pass'}  (status) {path} :: {text.strip().splitlines()[-1][:60]}")
                    if got != want:
                        bad.append(f"{r.ID} status {name} {path}: expected {'a hit' if want else 'no hit'}")
        bad += exempt_selftest(rules, tmp)
        bad += runner_selftest(tmp)
        bad += id_selftest(rules)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    for b in bad:
        print("SELFTEST FAIL " + b)
    print(f"selftest: {'RED' if bad else 'GREEN'} ({len(bad)} failures)")
    return 1 if bad else 0


def id_selftest(rules):
    """The loaded rules hold each id once, and `id_clashes` names a planted pair (two modules on one id) and nothing else."""
    import types
    bad = ["id clash " + c for c in id_clashes(rules)]
    planted = [types.SimpleNamespace(ID="RQ", __file__="rq_a.py"), types.SimpleNamespace(ID="RQ", __file__="rq_b.py"),
               types.SimpleNamespace(ID="RZ", __file__="rz_c.py")]
    got = id_clashes(planted)
    print(f"  ids    planted {'hit ' if got else 'pass'}  {got}")
    if got != ["RQ: rq_a.py, rq_b.py"]:
        bad.append(f"ids planted: got {got}, want ['RQ: rq_a.py, rq_b.py']")
    return bad


def exempt_selftest(rules, tmp):
    """Each exempt row's test passes at its file and symbol, and hits under another symbol name and in another file."""
    bad = ["exempt " + p for p in exempt.row_problems({r.ID for r in rules})]
    text, want = zip(*[("impl A {", None), ("    fn f(x: [u8; 4]) -> u8 {", "f"), ("        let y = { x[0] };", "f"),
                       ("        y", "f"), ("    }", "f"), ("    const C: [u32; 2] = [1, 2];", "C"), ("}", None),
                       ("pub(crate) struct S;", "S"), ("mod m {", None), ("    pub unsafe fn g() {}", "g"),
                       ("    static T: u8 = 0;", "T"), ("}", None), ("fn k() {", "k"), ("    let z = 1;", "k"), ("}", "k")])
    got = exempt.symbols(Src(G + "lowerv4/src/x.rs", "\n".join(text) + "\n"), range(1, len(text) + 1))
    print(f"  exempt symbols {'ok' if tuple(got[n] for n in range(1, len(text) + 1)) == want else 'WRONG'}")
    if tuple(got[n] for n in range(1, len(text) + 1)) != want:
        bad.append(f"exempt symbols: got {[got[n] for n in range(1, len(text) + 1)]}, want {list(want)}")
    by_id = {r.ID: r for r in rules}
    for row in exempt.ROWS:
        r = by_id.get(row[0])
        if r is None:
            continue
        for name, path, text, want in (("row", row[1], row[4], False), ("renamed", row[1], exempt.renamed(row), True),
                                       ("elsewhere", exempt.elsewhere(row), row[4], True)):
            ctx = Ctx(tmp)
            got = bool(row_hits(r, Src(path, text), ctx))
            print(f"  {r.ID:4} exempt  {'hit ' if got else 'pass'}  {name:9} {path}:{row[2]}")
            if got != want:
                bad.append(f"exempt {r.ID} {row[1]}:{row[2]} {name}: expected {'a hit' if want else 'no hit'}")
    return bad


def _write(root, path, text):
    p = os.path.join(root, path)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, "w") as fh:
        fh.write(text)


def _quiet_run(root, **kw):
    class Sink:
        def __init__(self):
            self.text = ""

        def write(self, s):
            self.text += s
    s = Sink()
    rc = run(root, quiet=True, out=s, **kw)
    return rc, s.text


def runner_selftest(tmp):
    bad = []
    clean = "#![deny(unsafe_code)]\npub mod x;\n"
    dirty = "pub fn f(p: *const u8) -> u8 { unsafe { *p } }\n"
    leg = G + "hosts/src/ops/window"

    def case(name, want_rc, files, legacy=None, **kw):
        root = os.path.join(tmp, "runner-" + name)
        for p, t in files.items():
            _write(root, p, t)
        if legacy is not None:
            _write(root, LEGACY, legacy)
        rc, text = _quiet_run(root, **kw)
        print(f"  run  {name:30} rc={rc}")
        if rc != want_rc:
            bad.append(f"runner {name}: rc {rc}, expected {want_rc}\n{text}")
        return root

    case("zero-files", 1, {"README": "x"})
    case("clean-tree", 0, {G + "hosts/src/lib.rs": clean})
    case("hit-outside-legacy", 1, {G + "hosts/src/lib.rs": clean, leg + "/a.rs": dirty})
    case("hit-on-legacy", 0, {G + "hosts/src/lib.rs": clean, leg + "/a.rs": dirty}, legacy=leg + "\n")
    case("legacy-subdir-not-covered", 1, {G + "hosts/src/lib.rs": clean, leg + "/sub/a.rs": dirty, leg + "/b.rs": dirty},
         legacy=leg + "\n")
    case("legacy-missing-dir", 1, {G + "hosts/src/lib.rs": clean}, legacy=G + "hosts/src/gone\n")
    case("legacy-clean-dir", 1, {G + "hosts/src/lib.rs": clean, leg + "/a.rs": "pub fn f() {}\n"}, legacy=leg + "\n")
    case("legacy-duplicate", 1, {G + "hosts/src/lib.rs": clean, leg + "/a.rs": dirty}, legacy=leg + "\n" + leg + "\n")
    touched = "// main's layout, ruling 2026-10-04 15:11Z\npub fn p() {}\n"
    case("on-touch-hit-is-legacy", 0, {G + "hosts/src/lib.rs": clean, G + "hosts/src/ops/sort/p.rs": touched})
    case("on-touch-keeps-no-legacy-line", 1, {G + "hosts/src/lib.rs": clean, G + "hosts/src/ops/sort/p.rs": touched},
         legacy=G + "hosts/src/ops/sort\n")
    # landings, in a scratch git repository
    if shutil.which("git"):
        root = os.path.join(tmp, "git")
        base_files = {G + "hosts/src/lib.rs": clean, leg + "/a.rs": dirty + "pub fn g() {}\n",
                      G + "hosts/src/ops/scan/b.rs": "pub fn h() {}\n", LEGACY: leg + "\n",
                      G + "hosts/src/ops/sort/p.rs": touched}
        for p, t in base_files.items():
            _write(root, p, t)
        for a in (["init", "-q"], ["add", "-A"], ["-c", "user.name=rules", "-c", "user.email=rules@invalid", "commit",
                                                  "-q", "-m", "base"]):
            git(root, *a)
        base = git(root, "rev-parse", "HEAD").decode().strip()

        def landing(name, want_rc, edits, removes=(), base_edits=None):
            b = base
            if base_edits:   # a parent with these files on top of the scratch base, for this case only
                for p, t in base_edits.items():
                    _write(root, p, t)
                git(root, "add", "-A")
                git(root, "-c", "user.name=rules", "-c", "user.email=rules@invalid", "commit", "-q", "-m", name)
                b = git(root, "rev-parse", "HEAD").decode().strip()
            for p, t in edits.items():
                _write(root, p, t)
            for p in removes:
                os.remove(os.path.join(root, p))
            rc, text = _quiet_run(root, base=b)
            print(f"  diff {name:30} rc={rc}")
            if rc != want_rc:
                bad.append(f"landing {name}: rc {rc}, expected {want_rc}\n{text}")
            git(root, "checkout", "-q", "--", ".")
            git(root, "clean", "-qfd")
            if b != base:
                git(root, "reset", "-q", "--hard", base)

        landing("added-hit-in-legacy", 1, {leg + "/a.rs": dirty + "pub fn g() -> u8 { unsafe { 0 } }\n"})
        landing("edited-hit-line-in-legacy", 0, {leg + "/a.rs": dirty.replace("*p", "*p + 0") + "pub fn g() {}\n"})
        landing("moved-hit-into-legacy-file", 0, {leg + "/a.rs": "pub fn g() {}\n", leg + "/c.rs": dirty})
        # D2638: one deleted hit line excuses one added line: a move OR an edit, never both (lockrows#408: the line counted as
        # the move's source and again as an old hit, so a new hit beside the move passed)
        landing("one-deleted-line-moved-and-edited", 1, {leg + "/a.rs": dirty.replace("*p", "*p.add(3)") + "pub fn g() {}\n",
                                                         leg + "/c.rs": dirty})
        landing("moved-and-edited-two-deleted-lines", 0, {leg + "/a.rs": dirty.replace("*p", "*p.add(3)") + "pub fn g() {}\n",
                                                          leg + "/c.rs": dirty, leg + "/b.rs": "pub fn n() {}\n"},
                base_edits={leg + "/b.rs": dirty})
        landing("moved-hit-to-clean-dir", 1, {leg + "/a.rs": "pub fn g() {}\n", G + "hosts/src/ops/scan/c.rs": dirty})
        landing("moved-hit-replaces-legacy-line", 0, {leg + "/a.rs": "pub fn g() {}\n",
                                                      G + "hosts/src/ops/scan/c.rs": dirty,
                                                      LEGACY: G + "hosts/src/ops/scan\n"})
        landing("moved-out-line-kept", 1, {leg + "/a.rs": "pub fn g() {}\n", G + "hosts/src/ops/scan/c.rs": dirty,
                                           LEGACY: leg + "\n" + G + "hosts/src/ops/scan\n"})
        landing("legacy-line-without-move", 1, {G + "hosts/src/ops/scan/b.rs": "pub fn h() {}\n" + dirty,
                                                LEGACY: leg + "\n" + G + "hosts/src/ops/scan\n"})
        landing("moved-then-edited", 1, {leg + "/a.rs": "pub fn g() {}\n",
                                         leg + "/c.rs": dirty.replace("*p", "*p.add(1)")})
        landing("split-adds-file-allow", 0, {leg + "/a.rs": "pub fn g() {}\n",
                                             leg + "/c.rs": "#![allow(unsafe_code)]\n" + dirty})
        landing("file-allow-without-unsafe", 1, {leg + "/c.rs": "#![allow(unsafe_code)]\npub fn k() {}\n"})
        landing("file-allow-with-added-unsafe", 1, {leg + "/c.rs": "#![allow(unsafe_code)]\n" +
                                                    dirty.replace("*p", "*p.add(2)")})
        sort = G + "hosts/src/ops/sort/"
        landing("on-touch-untouched-hit", 0, {sort + "p.rs": touched + "pub fn q() {}\n"})
        landing("on-touch-added-hit", 1, {sort + "p.rs": touched + "// (main 09-27): q\npub fn q() {}\n"})
        landing("on-touch-moved-hit", 1, {sort + "p.rs": "pub fn q() {}\n", sort + "m.rs": touched})
        landing("on-touch-moved-stripped", 0, {sort + "p.rs": "pub fn q() {}\n", sort + "m.rs": "// the layout\npub fn p() {}\n"})
        landing("on-touch-warn-only", 0, {sort + "p.rs": touched + "// (main 09-27): q\npub fn q() {}\n", G + "lint/RP-WARN": "x\n"})
        # LEDGER-DIRS: a ledger directory (one row per file, README skipped) and the single file it replaced read the same
        # rows, in the tree (Ctx.ledger) and in a parent (base_ledger_reader); a row's problem points at its row file
        led = G + "lint/EFFECTS-DEBT"
        rows = ["crates/gravity/a.rs\tf\tagg\tcalls", "crates/gravity/b/c.rs\tg\tsort\torder"]
        _write(root, led + "/README", "# header\n")
        for r in rows:
            _write(root, led + "/" + r.split("\t")[0][len(G):].replace("/", ".") + "@" + r.split("\t")[1], r + "\n")
        git(root, "add", "-A")
        git(root, "-c", "user.name=rules", "-c", "user.email=rules@invalid", "commit", "-q", "-m", "ledger dir")
        tip = git(root, "rev-parse", "HEAD").decode().strip()
        text, at = Ctx(root).ledger(led)
        want = "\n".join(rows) + "\n"
        got = (text, at(2), base_ledger_reader(root, tip)(led), base_ledger_reader(root, base)(led))
        if got != (want, (led + "/b.c.rs@g", 1), want, None):
            bad.append(f"ledger dir: read {got!r}")
        shutil.rmtree(os.path.join(root, led))
        _write(root, led, "# header\n" + want)
        git(root, "add", "-A")
        git(root, "-c", "user.name=rules", "-c", "user.email=rules@invalid", "commit", "-q", "-m", "ledger file")
        one = git(root, "rev-parse", "HEAD").decode().strip()
        text, at = Ctx(root).ledger(led)
        got = (text, at(3), base_ledger_reader(root, one)(led))
        if got != ("# header\n" + want, (led, 3), "# header\n" + want):
            bad.append(f"ledger file: read {got!r}")
        print(f"  run  {'ledger-dir-and-file':30} {'ok' if not bad or not bad[-1].startswith('ledger') else 'BAD'}")
        git(root, "reset", "-q", "--hard", base)
    else:
        bad.append("git not found: the landing cases did not run")
    return bad


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", default=None, help="the checkout (default: the repository holding this script)")
    ap.add_argument("--base", help="the parent commit: also check every added line (a landing)")
    ap.add_argument("--open-bugs", help="open D-ids (one per line, or BUGS-OPEN.tsv): R10 checks Bug(D-id) gaps")
    ap.add_argument("--tsv", help="write every hit to this TSV")
    ap.add_argument("--write-legacy", action="store_true", help="write LEGACY as the directories with a hit (step 6)")
    ap.add_argument("--quiet", action="store_true")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    if a.selftest:
        sys.exit(selftest())
    root = a.root or os.path.abspath(os.path.join(HERE, "../../../.."))
    bugs = load_open_bugs(a.open_bugs) if a.open_bugs else None
    sys.exit(run(root, a.base, bugs, a.tsv, a.write_legacy, a.quiet))


if __name__ == "__main__":
    main()
