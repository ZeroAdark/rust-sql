"""RC: a function outside the column-demand home decides which output columns of a plan node a node above reads (C5 (c),
reports/BUG-RECURRENCE-20261011.md: D3728, D3766, D3785, D3855, D3905, three local walks with three premises). The home
is decompose/src/facts/demand/: `reads_above(plan, attno)`, computed top-down once per statement. Every "null this
resjunk entry / drop this column / pin this column" decision reads it. A decision is found two ways:
- by name, in the decompose and lowerv4 sources: a `fn` whose name says it decides column demand (a token `unread` or
  `unkept`; `readers_of` / `readers_at`; a name ending `read_above`, `reads_above` or `dead_above`) and whose body reads
  a column (`varattno`, `resjunk`, `ColIdx`, `resno`, `attno`, `.col`): an initplan's or a slot's "unread" is not one;
- by construct, anywhere in the gravity sources: a function that looks up a plan node's parent (`parent_of(`,
  `fill_parents(`, `find(b, `, `.parents`) and reads a column's number or junk mark there (`varattno`, `resjunk`,
  `ColIdx`): a walk up the plan to a column's readers.
A function that asks the home (`reads_above(`, `demand::`) decides through it and is not flagged.

The registry, lint/DEMAND-DEBT (a directory, one row per file, as LIFETIME-DEBT), lists each such function not yet moved,
by path and name, with its owner lane and kind (null, drop, pin, walk), and the local walks the census of 2026-10-11 named
that neither detector sees. A detected function with no row fails. On a landing (--base) a row the parent lacks fails (the
list only shrinks), unless it replaces a row of the same function name the landing removes (a certified move or split).
A row whose function its file no longer defines is a note (delete it). The owner deletes a row when the decision reads
`reads_above`."""
import re

from common import G, ledger_loc, Src, generated, in_any, in_src, walk_sources
from rl_lifetime import functions

ID = "RC"
TITLE = "a function outside the column-demand home decides which columns a node above reads"
INSTEAD = "decompose facts::demand::reads_above (one top-down fact per statement); not yet moved: a lint/DEMAND-DEBT row"
GLOBAL = True
DEBT = G + "lint/DEMAND-DEBT"
_HOME = [G + "decompose/src/facts/demand/"]   # mod.rs (the fact) and readers.rs (the reader walks, C5 (c) step A)
SCOPE = in_src
NAME_SCOPE = (G + "decompose/src/", G + "lowerv4/src/")

KINDS = {"null", "drop", "pin", "walk"}
_TOKENS = {"unread", "unkept"}
_NAMES = {"readers_of", "readers_at"}
_SUFFIX = re.compile(r"(?:^|_)(?:read|reads|dead)_above$")
_PARENT = re.compile(r"(?<!fn )\b(?:parent_of|fill_parents)\s*\(|(?<!fn )\bfind\s*\(\s*b\s*,|\.parents\b")
_COLUMN = re.compile(r"\bvarattno\b|\bresjunk\b|ColIdx\b")
_COLUMN_NAMED = re.compile(r"\bvarattno\b|\bresjunk\b|ColIdx\b|\bresno\b|\battno\b|\.col\b")
_NEEDS = re.compile(r"parent_of|fill_parents|find|parents|unread|unkept|readers_|_above")
_ASKS = re.compile(r"\breads_above\s*\(|\bdemand::")


def is_home(p):
    return in_any(p, _HOME)


def named(name):
    return bool(_TOKENS & set(name.lower().split("_"))) or name in _NAMES or bool(_SUFFIX.search(name))


def deciders(src):
    """-> [(name, line)] of the functions `src` defines that decide column demand (see the module doc)."""
    t, fns = src.joined("code"), functions(src)
    out = set()
    if src.path.startswith(NAME_SCOPE):
        out |= {(n, line) for n, line, (a, b) in fns if named(n) and _COLUMN_NAMED.search(t, a, b)}
    both = [(n, line, (a, b)) for n, line, (a, b) in fns if _PARENT.search(t, a, b) and _COLUMN.search(t, a, b)]
    for n, line, (a, b) in both:
        # charged to the innermost function that holds both constructs (a nested fn's text is in its parent's body too)
        if not any(a <= a2 and b2 <= b and (a2, b2) != (a, b) for _, _, (a2, b2) in both):
            out.add((n, line))
    asks = {n for n, _, (a, b) in fns if _ASKS.search(t, a, b)}
    return sorted((x for x in out if x[0] not in asks), key=lambda x: (x[1], x[0]))


def defined(src):
    return {n for n, _, _ in functions(src)}


def parse_debt(text):
    """-> ({(path, fn): (line, owner, kind)}, [(line, problem)])."""
    rows, bad = {}, []
    for n, raw in enumerate(text.split("\n"), 1):
        s = raw.split("#", 1)[0].rstrip()
        if not s.strip():
            continue
        f = s.split("\t")
        if len(f) < 4 or not all(x.strip() for x in f[:4]):
            bad.append((n, "a row is path<TAB>fn<TAB>owner lane<TAB>kind<TAB>what it decides"))
            continue
        kind = f[3].strip()
        if kind not in KINDS:
            bad.append((n, f"kind {kind!r} is not one of {', '.join(sorted(KINDS))}"))
            continue
        key = (f[0].strip(), f[1].strip())
        if key in rows:
            bad.append((n, f"duplicate row {key[0]} {key[1]}"))
        rows[key] = (n, f[2].strip(), kind)
    return rows, bad


def check_tree(ctx):
    out, hits, files = [], {}, {}
    text, at = ctx.ledger(DEBT)
    rows, bad = parse_debt(text) if text is not None else ({}, [])
    listed = {p for p, _ in rows}
    for p in walk_sources(ctx.root):
        if not SCOPE(p) or is_home(p):
            continue
        t = ctx.read(p)
        if t is None or generated(t) or not (_NEEDS.search(t) or p in listed):
            continue
        src = Src(p, t)
        if p in listed:
            files[p] = defined(src)
        for name, line in deciders(src):
            hits.setdefault((p, name), line)
    out += [(*at(n), note) for n, note in bad]
    for (p, f), n in sorted(hits.items()):
        if (p, f) not in rows:
            out.append((p, n, f"`{f}` decides which columns a node above reads outside the column-demand home; "
                              f"instead: {INSTEAD}"))
    stale = [f"{ledger_loc(at, n)}: {p} `{f}` ({owner}) is not in its file: delete the row once it is gone (an entry queued ahead may add it)"
             for (p, f), (n, owner, _) in sorted(rows.items(), key=lambda kv: kv[1][0])
             if (p, f) not in hits and f not in files.get(p, set())]
    ctx.notes += [f"RC: {s}" for s in stale]
    base_text = ctx.base_ledger(DEBT) if ctx.base_ledger else None
    if base_text is not None:
        base, _ = parse_debt(base_text)
        removed = {bf for bp, bf in base if (bp, bf) not in rows}
        for (p, f), (n, owner, _) in sorted(rows.items(), key=lambda kv: kv[1][0]):
            if (p, f) not in base and f not in removed:
                out.append((*at(n), f"{p} `{f}` ({owner}) added: the list only shrinks (read facts::demand::reads_above)"))
    return out


# the planted failure is a new local walk up to a column's readers (D3785's premise, re-derived)
PLANTED_TOML = (G + "decompose/src/ops/sort/planted.rs",
                "fn sort_reads(b: &Builder<'_, '_>, p: &Plan<'_>, c: i16) -> bool {\n"
                "    parent_of(b, p).is_some_and(|(n, _)| n.as_sort().is_some_and(|s| s.sortColIdx.contains(&c)))\n}\n")
MORE_PLANTED_TOML = [(G + "lowerv4/src/ops/sort/planted.rs", "fn unread_tail(r: &RowVals, attno: i16) -> bool { r.col(attno).is_none() }\n")]
CLEAN_TOML = (G + "decompose/src/ops/sort/clean.rs",
              "fn sort_reads(b: &Builder<'_, '_>, p: &Plan<'_>, c: i16) -> bool { reads_above(b, p, c) != Read::No }\n"
              "fn held(b: &Builder<'_, '_>, p: &Plan<'_>) -> bool { parent_of(b, p).is_none() }\n")
