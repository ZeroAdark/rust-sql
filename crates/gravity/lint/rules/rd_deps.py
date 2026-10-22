"""RD: a function outside the dependency home keys or retires a catalog-derived cache by an epoch, a stamp or a hand-built
dependency list (construct C3, recorded dependencies: lanes/rxcursor/DESIGN-c3-deps.md). The home is gravity_deps
(crates/gravity/deps/): a lowering's recorded `Deps`, fed by the catalog caches themselves, which every cache of a
lowered program, a decomposed graph, a bound call or a catalog answer validates and retires by. Found by construct,
anywhere in the gravity sources outside the home:
- epoch: a backend-wide invalidation count read to key or retire such a cache (`relcache_epoch`, `support_epoch`,
  `front_epoch`, keysem's `epoch::now` / `epoch::current`, `catalog_epoch`);
- stamp: the shared tier's hand-built stamp (`catalog_stamp`, `catalog_stamp_read`, `stamp_now`, `plan_dependency_word`);
- composite: the hand-walked composite relations (`composite_rels`);
- note: the per-thread hand lists of what a lowering read (`note_row_rel`, `note_row_type`, `note_domain`, `domain_read`,
  `row_rel_read`, `forget_row_rels`);
- gen: the next epoch, whatever its name: a raw read of the invalidation counters and generations the home reads
  (l2cache `view_moves` / `current_gen` / `view_gen` / `prog_gen` / `row_gen`, catcache `inval_epoch`), or a syscache
  invalidation callback registered (`CacheRegisterSyscacheCallback`: every old epoch counted in one); a holder asks
  the home (`Held`, `Deps::current`, `Opaque` for state no recording sees);
- def: a definition of one of those (by name anywhere outside the home, and keysem's epoch module `now` / `current` /
  `moved`), so the registry reaches 0 only when the old mechanisms are deleted, not merely unused. The caches keyed by
  a GUC or the session (RowDescription D3535, the regex / locale caches D3532) are not catalog dependencies: not RD's.

The registry, lint/DEPS-DEBT (a directory, one row per file, named <path>@<fn>), lists each such function by path and
name with its owner lane and kinds (joined by '+'). A detected function with no row FAILS. On a landing (--base) a row
the parent lacks fails (the list only shrinks), unless it replaces a row of the same function name the landing removes.
A row whose function its file no longer defines is a note (delete it). The construct is done when the registry is
empty and the definitions are gone."""
import re

from common import G, ledger_loc, Src, generated, in_any, in_src, walk_sources
from rl_lifetime import functions

ID = "RD"
TITLE = "a function outside the dependency home keys or retires a catalog-derived cache by an epoch, stamp or hand list"
INSTEAD = "gravity_deps (a lowering's recorded Deps: `Deps::current`, the caches' notes); not yet moved: a lint/DEPS-DEBT row"
GLOBAL = True
DEBT = G + "lint/DEPS-DEBT"
_HOME = [G + "deps/"]
SCOPE = in_src

KINDS = {"epoch", "stamp", "composite", "note", "gen", "def"}
USES = [
    ("epoch", re.compile(r"(?<!fn )\b(?:relcache_epoch|support_epoch|front_epoch|catalog_epoch)\s*\(|\bepoch::(?:now|current)\s*\(")),
    ("stamp", re.compile(r"(?<!fn )\b(?:catalog_stamp|catalog_stamp_read|stamp_now|plan_dependency_word)\s*\(")),
    ("composite", re.compile(r"(?<!fn )\bcomposite_rels\s*\(")),
    ("note", re.compile(r"(?<!fn )\b(?:note_row_rel|note_row_type|note_domain|domain_read|row_rel_read|forget_row_rels)\s*\(")),
    ("gen", re.compile(r"\bl2cache::(?:view_moves|current_gen|view_gen|prog_gen|row_gen)\s*\(|\bcatcache::(?:graph::)?inval_epoch\s*\("
                       r"|\bCacheRegisterSyscacheCallback\s*\(")),
]
_NEEDS = re.compile(r"relcache_epoch|support_epoch|front_epoch|catalog_epoch|epoch::|catalog_stamp|stamp_now|plan_dependency_word"
                    r"|composite_rels|note_row_rel|note_row_type|note_domain|domain_read|row_rel_read|forget_row_rels"
                    r"|l2cache::|inval_epoch|CacheRegisterSyscacheCallback")
DEFS = {"relcache_epoch", "support_epoch", "front_epoch", "catalog_epoch", "catalog_stamp", "catalog_stamp_read", "stamp_now",
        "plan_dependency_word", "composite_rels", "note_row_rel", "note_row_type", "note_domain", "domain_read", "row_rel_read",
        "forget_row_rels"}
DEF_FILES = {G + "fn/src/typesem/keysem/epoch.rs": {"now", "current", "moved"}}


def is_home(p):
    return in_any(p, _HOME)


def users(src):
    """-> {(name, line): set of kinds} of the functions `src` defines that key or retire by an old mechanism; a construct
    is charged to the innermost function holding it."""
    t, fns = src.joined("code"), functions(src)
    out = {}
    own = DEF_FILES.get(src.path, set())
    for n, line, _ in fns:
        if n in DEFS or n in own:
            out.setdefault((n, line), set()).add("def")
    for kind, pat in USES:
        for m in pat.finditer(t):
            inner = [(n, line, b - a) for n, line, (a, b) in fns if a <= m.start() < b]
            if inner:
                n, line, _ = min(inner, key=lambda x: x[2])
                out.setdefault((n, line), set()).add(kind)
    return out


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
            bad.append((n, "a row is path<TAB>fn<TAB>owner lane<TAB>kind[+kind..]<TAB>what it keys or retires"))
            continue
        kind = f[3].strip()
        if not set(kind.split("+")) <= KINDS:
            bad.append((n, f"kind {kind!r} is not one or more of {', '.join(sorted(KINDS))} joined by '+'"))
            continue
        key = (f[0].strip(), f[1].strip())
        if key in rows:
            bad.append((n, f"duplicate row {key[0]} {key[1]}"))
        rows[key] = (n, f[2].strip(), kind)
    return rows, bad


def scan(ctx):
    """-> ({(path, fn): (line, kinds)}, {path: defined names}) over the tree, rows' files included."""
    text, _ = ctx.ledger(DEBT)
    rows, _ = parse_debt(text) if text is not None else ({}, [])
    listed = {p for p, _ in rows}
    hits, files = {}, {}
    for p in walk_sources(ctx.root):
        if not SCOPE(p) or is_home(p):
            continue
        t = ctx.read(p)
        if t is None or generated(t) or not (_NEEDS.search(t) or p in listed or p in DEF_FILES):
            continue
        src = Src(p, t)
        if p in listed:
            files[p] = defined(src)
        for (name, line), kinds in users(src).items():
            h = hits.setdefault((p, name), (line, set()))
            h[1].update(kinds)
    return hits, files


def check_tree(ctx):
    out = []
    text, at = ctx.ledger(DEBT)
    rows, bad = parse_debt(text) if text is not None else ({}, [])
    hits, files = scan(ctx)
    out += [(*at(n), note) for n, note in bad]
    for (p, f), (n, kinds) in sorted(hits.items()):
        if (p, f) not in rows:
            out.append((p, n, f"`{f}` keys or retires by {'+'.join(sorted(kinds))} outside the dependency home; instead: {INSTEAD}"))
    stale = [f"{ledger_loc(at, n)}: {p} `{f}` ({owner}) is not in its file: delete the row once it is gone"
             for (p, f), (n, owner, _) in sorted(rows.items(), key=lambda kv: kv[1][0])
             if (p, f) not in hits and f not in files.get(p, set())]
    ctx.notes += [f"RD: {s}" for s in stale]
    ctx.notes.append(f"RD: DEPS-DEBT {len(rows)} rows")
    base_text = ctx.base_ledger(DEBT) if ctx.base_ledger else None
    if base_text is not None:
        base, _ = parse_debt(base_text)
        removed = {bf for bp, bf in base if (bp, bf) not in rows}
        for (p, f), (n, owner, _) in sorted(rows.items(), key=lambda kv: kv[1][0]):
            if (p, f) not in base and f not in removed:
                out.append((*at(n), f"{p} `{f}` ({owner}) added: the list only shrinks (key and retire by gravity_deps)"))
    return out


PLANTED_TOML = (G + "exec/src/planted.rs", "fn file_key(k: u128) -> u128 { k ^ residency::relcache_epoch() as u128 }\n")
MORE_PLANTED_TOML = ((G + "hosts/src/planted.rs", "pub fn catalog_stamp(r: &[u32]) -> Option<u128> { r.first().map(|&x| x as u128) }\n"),
                     (G + "fn/src/planted.rs", "fn fresh(m: &mut u64) -> bool { let n = l2cache::view_moves(); core::mem::replace(m, n) == n }\n"),
                     (G + "kernels/src/planted.rs", "fn watch() { let _ = inval::invalidate::CacheRegisterSyscacheCallback(47, moved, datum::Datum::null()); }\n"))
CLEAN_TOML = (G + "exec/src/clean.rs", "fn still(d: &gravity_deps::Deps) -> bool { d.current() }\nfn now() -> u64 { 1 }\n"
              "fn lane(m: gravity_deps::Opaque) -> bool { m == gravity_deps::Opaque::now() }\n")
MORE_CLEAN_TOML = ((G + "deps/src/clean.rs", "pub fn relcache_epoch() -> u32 { gravity_l2cache::current_gen(0) }\n"),)
