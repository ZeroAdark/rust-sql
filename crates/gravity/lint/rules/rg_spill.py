"""RG: a breaker settled to grow past its grant outside the spill decider (construct C10, spill coverage). The home is
`gravity_program::spill` (crates/gravity/program/src/spill.rs): `plan` is the one constructor of a breaker's answer past
its grant (`Holds::Spills` or `Holds::Bounded` by a proof), and every debt is a `NoSpill` reason there. RG reads the
lowering and run-time sources (decompose, lowerv4, kernels, exec, hosts, parallel: `crates/gravity/<crate>/src/**`,
#[cfg(test)] items left out) and finds, by construct, charged to the innermost function:
- a breaker's spill settled to none (`spill: Spill::None`, `spill = Spill::None`, a tuple assignment ending in
  `Spill::None`), a hash build's partition bits settled to none (`spill: 0`, `spill = 0`), a keyed end kept resident
  (`end_resident(..)`);
- a grow on the uncapped road (`host_abi::region_grow`, `program::region_grow(..)`): a breaker past its grant there
  grows where PostgreSQL spills, unless its answer is bounded.
A function that names the decider (`NoSpill::`, `spill::plan` / `declared` / `Shape`, a `Shape::` answer, `end_holds`)
states its answer there and passes. Decompose's declarations (`breakers::declare`) are answered by `spill::declared`.
Any other site FAILS, but for the reviewed ones in `NOT_ROUTES` (a road whose home the lowering answers elsewhere).

The registry lint/SPILL-DEBT holds one row per `NoSpill` reason but `Unrouted` (a directory, one row per file named for
the reason): reason<TAB>site path<TAB>function<TAB>owner lane<TAB>bugs (or -)<TAB>what it settles<TAB>family<TAB>what a
user sees. NR-7 (the known limitations): the family is one of `FAMILIES`, the operator a user meets the reason under, or
`unreachable` when no SQL reaches it; what a user sees is one plain sentence naming no SQLSTATE (the refusal's error
form is open). A reason with no row, a row naming no reason, or a row short of either field FAILS. On a landing (--base)
a row the parent lacks fails (the list only shrinks), unless the parent has no ledger yet, or the row is a
SEED_CORRECTIONS reason the parent's spill.rs does not declare yet (each one main's word: a road the seed missed,
counted from now on). SPILL-DEBT n is the row count."""
import re

from common import G, ledger_loc, Src, generated, in_src, walk_sources
from rl_lifetime import functions

ID = "RG"
TITLE = "a breaker settled to grow past its grant outside the spill decider"
INSTEAD = "gravity_program::spill::plan (its answer through frame::iface::end_holds, or a NoSpill reason with its " \
          "lint/SPILL-DEBT row); the uncapped road names its debt (State::grow_debt, gravity_abi region_grow_debt)"
GLOBAL = True
DEBT = G + "lint/SPILL-DEBT"
HOME = G + "program/src/spill.rs"
CRATES = ("decompose", "lowerv4", "kernels", "exec", "hosts", "parallel")

# (path, function): why the site's answer is stated elsewhere, reviewed with the rule
NOT_ROUTES = {
    (G + "kernels/src/host_abi.rs", "region_grow"):
        "the IR's host export of the uncapped road itself (gravity_abi::program::region_grow)",
    (G + "lowerv4/src/ops/hashjoin/build.rs", "grow"):
        "a build of no partition bits grows here; BuildEnd::holds answers its NoSpill reason",
    (G + "kernels/src/ops/mergejoin/raw.rs", "grow"):
        "a merge inner run off the put road (m11_run_grow); face/slot_run.rs answers its NoSpill reason (MergeRun*)",
    (G + "kernels/src/par_hashjoin.rs", "breakers_bits"):
        "a hand program's third-phase scratch (its rows go to the sink; the driver stamps a spill answer for lowered "
        "programs only)",
}

SITE = re.compile(r"\bspill\s*[:=]\s*(?:gravity_abi::breaker::)?Spill::None\b"
                  r"|\bSpill::None\s*\)\s*;"
                  r"|\bspill\s*[:=]\s*0\b(?!\s*\.)"
                  r"|(?<!fn\s)\bend_resident\s*\("
                  r"|\bhost_abi::region_grow\b|\bprogram::region_grow\s*\(")
ROUTED = re.compile(r"\bNoSpill::|\bspill::(?:plan|declared|Shape|NoSpill)\b|\bShape::(?:Grows|Constant|PgHolds|Spills)\b"
                    r"|\bend_holds\s*\(")
_NEEDS = re.compile(r"Spill::None|\bspill\s*[:=]\s*0\b|end_resident|region_grow")
# reason: main's word admitting its row once, as the reason arrives (the shrink-only check holds for it after)
SEED_CORRECTIONS = {
    "AggOwnerAll": "main 2026-10-12T06:07:48Z (1): D3582's owner-phase pin past W and G, a road the seed's counter did not see",
    "AggPartitionPastGrant": "main 2026-10-12T06:16:02Z (Q1): a batched owner's partition past the grant stays one level; "
                             "the recursive re-split stays a C10 row",
    "LoadSealPastLine": "main 2026-10-13 ~18:1xZ (user ruling): COPY into pgrcolumnar is SPILL DEBT until it can spill "
                        "(D4370); D4365's ladder gate refuses by this name",
}
# NR-7: field 7's operator families (the limitations list groups by them), else `unreachable`
FAMILIES = ("hash join", "nested loop", "merge join", "hash aggregate", "plain aggregate", "set operation", "bulk load")
UNREACHABLE = "unreachable"
_SQLSTATE = re.compile(r"\bSQLSTATE\b|\b[0-9][0-9A-Z]{4}\b")
VARIANT = re.compile(r"^\s{4}([A-Z][A-Za-z0-9]*),\s*$", re.M)


def sites(src):
    """-> {(function, line)}: the functions of `src` settling a breaker past its grant without naming the decider."""
    out = set()
    code = src.joined("code")
    fns = functions(src)
    for m in SITE.finditer(code):
        inner = [(n, ln, a, b) for n, ln, (a, b) in fns if a <= m.start() < b]
        if inner:
            n, ln, a, b = min(inner, key=lambda x: x[3] - x[2])
            if not ROUTED.search(code, a, b):
                out.add((n, ln))
        else:
            out.add(("<item>", code.count("\n", 0, m.start()) + 1))
    return out


def reasons(text):
    """-> the `NoSpill` reasons spill.rs declares (`Unrouted`, a Bug's name, has no row), in order; None without one."""
    m = re.search(r"pub enum NoSpill\s*\{(.*?)\n\}", text, re.S)
    if not m:
        return None
    return [v for v in VARIANT.findall(m.group(1)) if v != "Unrouted"]


def parse_debt(text):
    """-> ({reason: (line, owner)}, [(line, problem)])."""
    rows, bad = {}, []
    for n, raw in enumerate(text.split("\n"), 1):
        s = raw.split("#", 1)[0].rstrip()
        if not s.strip():
            continue
        f = s.split("\t")
        if len(f) < 8 or not all(x.strip() for x in f[:8]):
            bad.append((n, "a row is reason<TAB>site path<TAB>function<TAB>owner lane<TAB>bugs or -<TAB>what it settles"
                           "<TAB>family or unreachable<TAB>what a user sees (NR-7)"))
            continue
        if f[6].strip() not in FAMILIES + (UNREACHABLE,):
            bad.append((n, f"field 7 `{f[6].strip()}` is no operator family ({', '.join(FAMILIES)}) nor `{UNREACHABLE}`"))
        if _SQLSTATE.search(f[7]):
            bad.append((n, "field 8 names an SQLSTATE: say what the user sees, whichever error form the refusal takes"))
        key = f[0].strip()
        if key in rows:
            bad.append((n, f"duplicate row {key}"))
        rows[key] = (n, f[3].strip())
    return rows, bad


def reasons_of(text):
    """-> the reasons a ledger's rows name, whatever their fields (a parent's rows may predate a field: NR-7)."""
    return {s.split("\t", 1)[0].strip() for s in (raw.split("#", 1)[0] for raw in text.split("\n")) if s.strip()}


def scan(ctx):
    """-> {(path, function): line} over the lowering and run-time sources."""
    hits = {}
    for p in walk_sources(ctx.root):
        if not in_src(p) or p == HOME or not any(p.startswith(G + c + "/src/") for c in CRATES):
            continue
        t = ctx.read(p)
        if t is None or generated(t) or not _NEEDS.search(t):
            continue
        for name, line in sites(Src(p, t)):
            if (p, name) not in NOT_ROUTES:
                hits.setdefault((p, name), line)
    return hits


def check_tree(ctx):
    out = []
    for (p, name), n in sorted(scan(ctx).items()):
        out.append((p, n, f"`{name}` settles a breaker past its grant itself; instead: {INSTEAD}"))
    home = ctx.read(HOME)
    want = reasons(home) if home is not None else None
    text, at = ctx.ledger(DEBT)
    rows, bad = parse_debt(text) if text is not None else ({}, [])
    out += [(*at(n), note) for n, note in bad]
    if want is not None:
        for r in want:
            if r not in rows:
                out.append((HOME, 1, f"NoSpill::{r} has no lint/SPILL-DEBT row (reason, site, function, owner, bugs, what)"))
        for r, (n, owner) in sorted(rows.items(), key=lambda kv: kv[1][0]):
            if r not in want:
                out.append((*at(n), f"SPILL-DEBT row {r} ({owner}) names no NoSpill reason: delete it with its reason"))
    if rows:
        ctx.notes.append(f"RG: SPILL-DEBT {len(rows)} rows")
    base_text = ctx.base_ledger(DEBT) if ctx.base_ledger else None
    if base_text is not None:
        base = reasons_of(base_text)
        base_home = ctx.base_read(HOME) if ctx.base_read else None
        base_reasons = set(reasons(base_home) or []) if base_home is not None else None
        for r, (n, owner) in sorted(rows.items(), key=lambda kv: kv[1][0]):
            if r not in base:
                if r in SEED_CORRECTIONS and base_reasons is not None and r not in base_reasons:
                    ctx.notes.append(f"RG: seed correction {r} ({SEED_CORRECTIONS[r]})")
                    continue
                out.append((*at(n), f"SPILL-DEBT row {r} ({owner}) added: the list only shrinks (spill the breaker, or prove its bound)"))
    return out


_HOME_TEXT = "pub enum NoSpill {\n    /// a\n    BuildGuarded,\n    /// b\n    Unrouted,\n}\n"
_ROW6 = "BuildGuarded\tcrates/gravity/lowerv4/src/ops/hashjoin/build.rs\tmake\tlifetime\t-\ta guarded build keeps its partitions"
_ROW = _ROW6 + "\thash join\tA guarded hash join keeps its table in memory.\n"

# the planted failure settles a lowered end to no spill without naming the decider; the clean one states its answer
PLANTED_TOML = (G + "lowerv4/src/ops/planted.rs",
                "fn desc(mut d: Pipeline) -> Pipeline {\n    if let Some(e) = breaker_of_mut(&mut d) {\n"
                "        *e = Breaker { spill: Spill::None, ..*e };\n    }\n    d\n}\n")
CLEAN_TOML = (G + "lowerv4/src/ops/clean.rs",
              "fn desc(mut d: Pipeline) -> Pipeline {\n    if let Some(e) = breaker_of_mut(&mut d) {\n"
              "        *e = Breaker { spill: Spill::None, ..*e };\n    }\n"
              "    end_holds(d, Shape::Grows(NoSpill::TwoPassPass1))\n}\n")
MORE_PLANTED_TOML = [
    (G + "kernels/src/ops/planted_grow.rs",       # the uncapped road, its home answered nowhere
     "unsafe fn grow(ctx: *mut Ctx) -> u64 {\n    gravity_abi::program::region_grow(ctx, 64, 0, 0)\n}\n"),
    {"files": [(HOME, _HOME_TEXT)], "want": "NoSpill::BuildGuarded has no lint/SPILL-DEBT row"},
    {"files": [(HOME, _HOME_TEXT), (DEBT + "/BuildGuarded", _ROW)], "base": {DEBT: ""},
     "want": "SPILL-DEBT row BuildGuarded (lifetime) added"},
    # NR-7: a row short of its family and its user sentence, a family that is none, a sentence naming an SQLSTATE
    {"files": [(HOME, _HOME_TEXT), (DEBT + "/BuildGuarded", _ROW6 + "\n")], "want": "what a user sees (NR-7)"},
    {"files": [(HOME, _HOME_TEXT), (DEBT + "/BuildGuarded", _ROW6 + "\thashjoin\tIt fails.\n")],
     "want": "is no operator family"},
    {"files": [(HOME, _HOME_TEXT), (DEBT + "/BuildGuarded", _ROW6 + "\thash join\tIt fails with 53200.\n")],
     "want": "field 8 names an SQLSTATE"},
]
MORE_CLEAN_TOML = [
    (G + "kernels/src/ops/clean_grow.rs",         # the road named as a debt
     "unsafe fn grow(ctx: *mut Ctx) -> u64 {\n    gravity_abi::program::region_grow_debt(ctx, 64, 0, 0, 3)\n}\n"),
    {"files": [(HOME, _HOME_TEXT), (DEBT + "/BuildGuarded", _ROW)]},
    # NR-7: a parent whose row predates fields 7 and 8 holds the row all the same (gaining fields adds no row)
    {"files": [(HOME, _HOME_TEXT), (DEBT + "/BuildGuarded", _ROW)], "base": {DEBT: _ROW6 + "\n"}},
]
