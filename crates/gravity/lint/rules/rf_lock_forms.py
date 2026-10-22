"""RF: a function outside the one lock plan decides a row lock's form, role, census, Program.locks entries or the one
write plan (construct C11 "one lock plan": lanes/wfrefuse/row-lock-forms.md §5 with its review's changes,
row-lock-forms-ATTACK.md §7; main 2026-10-12T15:41:55Z, 16:05:32Z). The home is the lowering's ops/lockrows/plan.rs:
`plan(gx) -> LockPlan`, once per program before the first make, whose Answer per lock op names its form, role and entry
and whose WriteOwner names the make that keeps the write plan. A decision is found by construct anywhere in the lowering
crate outside the home and frame/px.rs (the context's definition):
- locks: a Program.locks write on the lowering context (`.locks = `, `.locks.push / extend / insert / clear / truncate /
  retain / drain`) or the INITMULTI handshake (`init_multi`: lock_init.rs mark to marks.rs and program.rs);
- write: the one write plan (`px.write`: a second claimant's test, a write, a share test, a read past the plan);
- role: a role decider (`lock_init::mark`, `set_lock::mark`, `pair_mark`, `claim_mark`, `write_pair`);
- census: a census helper over the statement's other pipelines (`plain_inits`, `pair_inits`, `sub_inits`, `subs_inits`,
  `init_member`, `ctes_only`, `cte_whole`, `cte_claims`, `init_pipelines`, `ctes_beside`, `alone_with`, `tail_sets`,
  `above_only`, `above_feeds`, `beside_drained`, `lock_beside_drained`, `after_emit`);
- shape: a call of a lock form's shape test (the make chain's: `breaker_limit`, `sort_tail`, `ref_copy`, `ref_limit`,
  `merge_tail`, `ref_chain`, `ref_build`, `family_limit`, `sort_claim`, the rerun/epq forms `set_lock::form`,
  `sub_lock::form`, `ref_nest::{form, chain_form, sort_form, copy_nest_form, lock_form, chain_lock_form, pair_form}`,
  `ref_anti::{form, marks}`, `ref_copy::probe_form`, `sort_chain::form`; the site tests `several`, `over_breaker`,
  `family_breaker`, `nest_outer`, `one_site`, `set_site`, `own_leaf`, `cte_site`, `subs_in_ir`, a call or a fn value
  passed as an argument);
- count: a pipeline-count comparison (`ps.len() ==` / `!=`, `pipelines().len() ==` / `!=`) in ops/lockrows/ or
  rerun/epq/, the local census sums `Census::rest` replaces.
Name-based by design: the review's type rule (Px::locks and Px::write private to the plan's module, E2 / E3) is what
stops a re-derived classifier; this rule keeps the debt visible and only shrinking until then.

ERROR MODE from the seed (E0). The registry, lint/LOCK-FORM-DEBT (a directory, one row per file named
`<path past crates/gravity/, '/' as '.'>@<fn>`), lists each such function by path and name with its owner lane, its
kinds (several joined by '+') and what it decides; a row covers that file's functions of that name for its kinds only. A
detected function with no row, or with a kind its row lacks, FAILS. On a landing (--base) a row the parent lacks fails
(the list only shrinks), unless it replaces a row of the same function name the landing removes (a certified move or
split); a row whose kinds grew fails too. A row whose function no longer decides is a note (delete it). Module-level
constructs are not charged (only uses inside a function are). C11 is DONE at 0 rows (E3)."""
import os
import re

from common import G, L, ledger_loc, Src, generated, in_any
from rl_lifetime import functions

ID = "RF"
TITLE = "a function outside the one lock plan decides a row lock's form, role, census, Program.locks or write plan"
INSTEAD = "ops/lockrows/plan.rs (C11: the LockPlan's Answer for the lock op, its Census, its WriteOwner); not yet " \
          "moved: a lint/LOCK-FORM-DEBT row"
GLOBAL = True
DEBT = G + "lint/LOCK-FORM-DEBT"
_HOME = L("ops/lockrows/plan.rs") + L("frame/px.rs")
_SCOPE = L("")
_LOCAL = L("ops/lockrows/") + L("rerun/epq/")

KINDS = ("locks", "write", "role", "census", "shape", "count")
_CENSUS = (r"plain_inits|pair_inits|sub_inits|subs_inits|init_member|ctes_only|cte_whole|cte_claims|init_pipelines"
           r"|ctes_beside|alone_with|tail_sets|above_only|above_feeds|beside_drained|lock_beside_drained|after_emit")
_SITE = r"several|over_breaker|family_breaker|nest_outer|one_site|set_site|own_leaf|cte_site|subs_in_ir"
_FORM = r"breaker_limit|sort_tail|ref_copy|ref_limit|merge_tail|ref_chain|ref_build|family_limit|sort_claim"
_QUAL = (r"(?:set_lock|sub_lock)::form|ref_nest::(?:form|chain_form|sort_form|copy_nest_form|lock_form|chain_lock_form"
         r"|pair_form)|ref_anti::(?:form|marks)|ref_copy::probe_form|sort_chain::form")
# (kind, pattern, local): a definition (`fn name`) or a method (`.name(`) is never a use; a `local` pattern counts only in
# ops/lockrows/ and rerun/epq/, where the unqualified names are the lock plan's (rowop_epq.rs has a subs_in_ir of its own)
USES = [
    ("locks", re.compile(r"\.locks\s*=(?!=)|\.locks\.(?:push|extend|insert|clear|truncate|retain|drain)\s*\(|\binit_multi\b"), False),
    ("write", re.compile(r"\bpx\.write\b"), False),
    ("role", re.compile(r"\b(?:lock_init|set_lock)::mark\s*\(|(?<![.\w])(?:pair_mark|claim_mark|write_pair)\s*\("), False),
    ("census", re.compile(r"(?<![.\w])(?:" + _CENSUS + r")\s*\("), False),
    ("shape", re.compile(r"\b(?:" + _QUAL + r")\s*\(|\blockrows::(?:\w+::)*(?:" + _FORM + "|" + _SITE + r")\b"), False),
    ("shape", re.compile(r"(?<![.\w:])(?:" + _FORM + "|" + _SITE + r")\s*\(|[,(]\s*(?:" + _SITE + r")\s*(?=[,)])"), True),
    ("count", re.compile(r"\bps\.len\(\)\s*[!=]=|[!=]=\s*ps\.len\(\)|\bpipelines\(\)\.len\(\)\s*[!=]=|[!=]=\s*(?:\w+\.)?pipelines\(\)\.len\(\)"), True),
]
_NEEDS = re.compile(r"locks|init_multi|px\.write|mark|write_pair|inits|init_member|ctes_only|cte_|init_pipelines|alone_with"
                    r"|tail_sets|above_|beside_drained|after_emit|" + _SITE + "|" + _FORM + r"|::form|probe_form|ref_anti::marks|len\(\)")


def is_home(p):
    return in_any(p, _HOME)


def in_scope(p):
    return in_any(p, _SCOPE) and not is_home(p)


def row_name(path, fn):
    """The row file's name: the path past crates/gravity/ with '/' as '.', '@', the function."""
    return path[len(G):].replace("/", ".") + "@" + fn


def deciders(src):
    """-> {(name, line): set of kinds} of the functions `src` defines that decide part of a lock's plan (see the module
    doc); a construct is charged to the innermost function holding it."""
    t, fns = src.joined("code"), functions(src)
    out = {}
    for kind, pat, local in USES:
        if local and not in_any(src.path, _LOCAL):
            continue
        for m in pat.finditer(t):
            inner = [(n, line, b - a) for n, line, (a, b) in fns if a <= m.start() < b]
            if inner:
                n, line, _ = min(inner, key=lambda x: x[2])
                out.setdefault((n, line), set()).add(kind)
    return out


def defined(src):
    return {n for n, _, _ in functions(src)}


def parse_debt(text):
    """-> ({(path, fn): (line, owner, kinds)}, [(line, problem)])."""
    rows, bad = {}, []
    for n, raw in enumerate(text.split("\n"), 1):
        s = raw.split("#", 1)[0].rstrip()
        if not s.strip():
            continue
        f = s.split("\t")
        if len(f) < 5 or not all(x.strip() for x in f[:5]):
            bad.append((n, "a row is path<TAB>fn<TAB>owner lane<TAB>kind[+kind..]<TAB>what it decides"))
            continue
        kinds = set(f[3].strip().split("+"))
        if not kinds <= set(KINDS):
            bad.append((n, f"kind {f[3].strip()!r} is not one or more of {', '.join(KINDS)} joined by '+'"))
            continue
        key = (f[0].strip(), f[1].strip())
        if key in rows:
            bad.append((n, f"duplicate row {key[0]} {key[1]}"))
        rows[key] = (n, f[2].strip(), kinds)
    return rows, bad


def sources(root):
    """The lowering crate's .rs files under src/ (both of its names), repository-relative."""
    out = []
    for base in _SCOPE:
        d = os.path.join(root, base)
        if not os.path.isdir(d):
            continue
        for dp, dn, fn in os.walk(d):
            dn[:] = sorted(x for x in dn if x != "target")
            out += [os.path.relpath(os.path.join(dp, f), root).replace(os.sep, "/") for f in sorted(fn) if f.endswith(".rs")]
    return out


def scan(ctx):
    """-> ({(path, fn): (line, kinds)}, {path: defined names}) over the lowering crate, rows' files included."""
    text, _ = ctx.ledger(DEBT)
    rows, _ = parse_debt(text) if text is not None else ({}, [])
    listed = {p for p, _ in rows}
    hits, files = {}, {}
    for p in sources(ctx.root):
        if not in_scope(p):
            continue
        t = ctx.read(p)
        if t is None or generated(t) or not (_NEEDS.search(t) or p in listed):
            continue
        src = Src(p, t)
        if p in listed:
            files[p] = defined(src)
        for (name, line), kinds in deciders(src).items():
            h = hits.setdefault((p, name), (line, set()))
            h[1].update(kinds)
    return hits, files


def seed(ctx, owner="isojoin"):
    """-> {row file name: row line} for every function the rule finds (E0's seed; `run.py` never calls it)."""
    hits, _ = scan(ctx)
    return {row_name(p, f): f"{p}\t{f}\t{owner}\t{'+'.join(k for k in KINDS if k in kinds)}\t{what(kinds)}"
            for (p, f), (_, kinds) in sorted(hits.items())}


_WHAT = {"locks": "writes Program.locks or the INITMULTI handshake", "write": "claims or reads the one write plan",
         "role": "decides a lock's role (the pair's A / B, a claim, a set)", "census": "counts the statement's other pipelines by a census helper",
         "shape": "tests a lock form's shape", "count": "compares the pipeline count by a local sum"}


def what(kinds):
    return "; ".join(_WHAT[k] for k in KINDS if k in kinds)


def check_tree(ctx):
    out = []
    text, at = ctx.ledger(DEBT)
    rows, bad = parse_debt(text) if text is not None else ({}, [])
    hits, files = scan(ctx)
    out += [(*at(n), note) for n, note in bad]
    for (p, f), (n, kinds) in sorted(hits.items()):
        row = rows.get((p, f))
        if row is None:
            out.append((p, n, f"`{f}` decides a lock's {'+'.join(k for k in KINDS if k in kinds)} outside the one lock plan; instead: {INSTEAD}"))
        elif not kinds <= row[2]:
            more = "+".join(k for k in KINDS if k in kinds - row[2])
            out.append((p, n, f"`{f}` decides a lock's {more} past its lint/LOCK-FORM-DEBT row ({'+'.join(k for k in KINDS if k in row[2])}); instead: {INSTEAD}"))
    stale = [f"{ledger_loc(at, n)}: {p} `{f}` ({owner}) no longer decides: delete the row once it is gone (an entry queued ahead may add it)"
             for (p, f), (n, owner, _) in sorted(rows.items(), key=lambda kv: kv[1][0])
             if (p, f) not in hits and f not in files.get(p, set())]
    ctx.notes += [f"RF: {s}" for s in stale]
    base_text = ctx.base_ledger(DEBT) if ctx.base_ledger else None
    if base_text is not None:
        base, _ = parse_debt(base_text)
        removed = {bf for bp, bf in base if (bp, bf) not in rows}
        for (p, f), (n, owner, kinds) in sorted(rows.items(), key=lambda kv: kv[1][0]):
            if (p, f) not in base and f not in removed:
                out.append((*at(n), f"{p} `{f}` ({owner}) added: the list only shrinks (decide it in ops/lockrows/plan.rs)"))
            elif (p, f) in base and not kinds <= base[(p, f)][2]:
                out.append((*at(n), f"{p} `{f}` ({owner}) widened to {'+'.join(k for k in KINDS if k in kinds)}: a row's kinds only shrink"))
    return out


_LR = G + "lowerv4/src/ops/lockrows/"
_EPQ = G + "lowerv4/src/rerun/epq/"
_ROW = G + "lint/LOCK-FORM-DEBT/"
# the planted failures: a Program.locks write past the plan (E0's selftest); a census sum in rerun/epq; a row a landing
# adds; a listed function that takes a kind its row lacks. The clean cases: a make that reads the plan's answer; a
# listed function inside its kinds.
PLANTED_TOML = (_LR + "planted.rs", "fn claim(px: &mut Px<'_, '_>, s: LockSite) { px.locks = vec![s]; }\n")
MORE_PLANTED_TOML = (
    (_EPQ + "planted.rs", "fn form(gx: &Gx<'_, '_>, n: usize) -> bool { let ps = gx.pipelines(); ps.len() == 2 + n }\n"),
    {"files": [(_LR + "planted.rs", "fn b_role(px: &mut Px<'_, '_>, k: usize) -> bool { pair_mark(px, k, 1, 0) }\n"),
               (_ROW + "lowerv4.src.ops.lockrows.planted.rs@b_role", _LR + "planted.rs\tb_role\tisojoin\trole\tdecides B's role\n")],
     "base": {DEBT: ""}, "want": "added"},
    {"files": [(_LR + "planted.rs", "fn b_role(px: &mut Px<'_, '_>, k: usize) -> bool { px.locks.push(k); pair_mark(px, k, 1, 0) }\n"),
               (_ROW + "lowerv4.src.ops.lockrows.planted.rs@b_role", _LR + "planted.rs\tb_role\tisojoin\trole\tdecides B's role\n")],
     "want": "past its"},
)
CLEAN_TOML = (_LR + "clean.rs", "fn make(px: &mut Px<'_, '_>, k: usize) -> Answer { px.plan.answer(k) }\n")
MORE_CLEAN_TOML = (
    {"files": [(_LR + "clean.rs", "fn b_role(px: &mut Px<'_, '_>, k: usize) -> bool { pair_mark(px, k, 1, 0) }\n"),
               (_ROW + "lowerv4.src.ops.lockrows.clean.rs@b_role", _LR + "clean.rs\tb_role\tisojoin\trole\tdecides B's role\n")]},
    (G + "lowerv4/src/ops/sort/clean.rs", "fn rows(ps: &[Pipeline<'_>]) -> bool { ps.len() == 2 }\n"),
)
