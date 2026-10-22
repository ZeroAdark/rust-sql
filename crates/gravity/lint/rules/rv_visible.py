"""RV: a function outside the effects home that decides volatility, raise-freedom or row-order visibility (the user,
10-03: one home for "is this visible to the user?"; FILE-MAP lowerv4/src/effects/visible.rs). The lowering half asks
`effects::visible::{order_visible, calls_visible, error_visible}` or the effects primitives; the planning half lives in
decompose/src/effects/ and decompose's facts. A decision is found by its name (a name heuristic, backed by the registry
below): a `fn` returning `bool` whose name says it decides (`volatile`, `acts`, `quiet`, `loud`, `raises`, `raising`, a
`raise_free` / `can_raise` / `may_raise`, `order_free` / `walk_free` / `sort_free`, `first_row`, or a visible-check name).
Code emitters (a `FunctionBuilder` parameter) are not decisions.

The registry, lint/EFFECTS-DEBT (a directory, one row per file), lists each such function not yet moved, by path and name, with its owner lane: a row
covers that file's functions of that name. A deciding function with no row fails. On a landing (--base) a row the parent
lacks fails (the list only shrinks), unless it replaces a row of the same function name the landing removes (a certified
move or split). A row whose function no longer decides is a note, not a failure (delete it): another lane's landing that
moves or retypes a listed function must not fail on this list. A moved function keeps its row: a deciding function with
no row is covered by a row of the same name whose own file no longer holds it (one row, one site; a note to re-key it).

Vocabulary 2 (C1, one effects answer): the names RV reads also say `calm`, `user`, `calls`, `act`, `pure`, `side`,
`raise_only`, `may_act`, and a decider may return `Option<bool>`, `Side`, `Effects` or `Found` (deciders outside the home
that vocabulary 1 never read). Two more kinds of site need a row:
- RW, a re-walk: a lowerv4 decider (the home's included) whose body walks an expression again (`of_expr`,
  `raise_free`, `expr::walk`, `as_func_expr` ..) instead of reading the node's `Effects`; the row goes when the decider
  becomes a function of (Effects, Departure, its strategy);
- RQ, a builtin list: integer oids listed (a const / static array, an array literal, a `matches!` of two or more) in an
  effects home file or inside a deciding function: builtin facts live in `types_core::quiet` (the one table).
A row's kind says which (order, calls, error, calls+error, walk, list). The widening landing registers today's sites
once: a parent ledger without the `# vocabulary 2` line lets its rows in; after that the list only shrinks again."""
import re

from common import G, ledger_loc, Src, generated, home, in_any, in_src, walk_sources

ID = "RV"
TITLE = "a function outside the effects home decides volatility, raise-freedom or row-order visibility"
INSTEAD = "effects::visible::{order_visible, calls_visible, error_visible} or the effects primitives (lowering), " \
          "decompose effects / facts (planning), the node's Effects (no second walk), types_core::quiet (builtin " \
          "facts); not yet moved: a lint/EFFECTS-DEBT row"
GLOBAL = True
DEBT = G + "lint/EFFECTS-DEBT"
# expr's catalog.rs holds the pg_proc row type gravity_expr reads below decompose (R1's HOME names it too); its call bit
# (FuncFacts::call_acts) is decided there, beside the row (main 2026-10-05T00:08:56Z, EFF-EXPR).
_HOME = home("effects", "facts") + [G + "expr/src/catalog.rs"]
SCOPE = in_src

TOKENS = {"volatile", "acts", "quiet", "loud", "raises", "raising"}
PAIRS = ("raise_free", "can_raise", "may_raise", "order_free", "walk_free", "sort_free", "first_row")
NAMES = {"order_visible", "calls_visible", "error_visible"}
_NEEDS = re.compile(r"volatile|acts|quiet|loud|raise|_free|first_row|_visible")
_FN = re.compile(r"\bfn\s+([A-Za-z_]\w*)")


def is_home(p):
    return in_any(p, _HOME)


# Vocabulary 2 (the docstring): more names, more return types. Codegen crates (backend: the TPDE frontends; ir) decide
# nothing about a plan's effects.
TOKENS2 = TOKENS | {"calm", "user", "calls", "act", "pure", "impure"}
PAIRS2 = PAIRS + ("raise_only", "may_act", "user_code", "user_fn", "user_call", "side_calls", "own_side", "side_by", "notice")
RET2 = re.compile(r"^(?:bool|Option<bool>|(?:\w+::)*(?:Side|Effects|Found))$")
_NOT_PLAN = [G + "backend/", G + "ir/"]
VOCAB2 = "# vocabulary 2"
_NEEDS2 = re.compile(r"volatile|acts?\b|act_|quiet|loud|raise|_free|first_row|_visible|calm|user|calls|pure|side|notice|\bconst\b|matches!")
# An expression walked again (RW): the walkers of both crates and the node accessors a per-call reading starts from.
_WALK = re.compile(r"\bof_expr\s*\(|\bof_node\s*\(|\bquiet_node\s*\(|\bof_list\s*\(|\bexpr::walk\s*\(|\bexpr::kids\s*\("
                   r"|\bwalk\s*\(|\braise_free_each\s*\(|\braise_free\s*\(|\bexpr_quiet\s*\(|\bexpr_acts\s*\("
                   r"|\bvolatile\s*\(|\buser_fn\s*\(|\.as_func_expr\(\)|\.as_op_expr\(\)")
_EFFECTS_HOME = home("effects", "facts")
_GENERATED_HOME = [G + "decompose/src/effects/procfacts.rs"]
_RW_CRATE = G + "lowerv4/src/"   # the lowering half (b) moves onto the node's Effects
_OIDS = re.compile(r"\b[1-9]\d{1,4}\b")


def decides2(name, ret):
    n = name.lower()
    named = n in NAMES or bool(TOKENS2 & set(n.split("_"))) or any(p in n for p in PAIRS2)
    # a bare `side` names a join side unless the function answers the hash join's side class (`Side`)
    sided = "side" in n.split("_") and re.search(r"(?:^|::)Side$", ret) is not None
    return (named or sided) and bool(RET2.match(ret))


def _oids(text):
    return len([x for x in _OIDS.findall(text) if int(x) >= 16])


def _oid_lists(t):
    """-> [(start, label)] of integer-oid lists in code text `t`: a const / static array of two or more, an array literal
    or tuple literal starting with two or more, a `matches!` of two or more (16 and up: below are small counts and widths,
    not oids)."""
    out = []
    for m in re.finditer(r"\b(?:const|static)\s+([A-Z_][A-Z0-9_]*)\s*:[^=;]*\[[^=;]*=", t):
        j = t.find(";", m.end())
        if _oids(t[m.end():j]) >= 2:
            out.append((m.start(), m.group(1)))
    for m in re.finditer(r"\[\s*\d+\s*(?:,\s*\d+\s*)+[,\]]|\(\s*\d+\s*(?:,\s*\d+\s*)+,?\s*\)", t):
        if _oids(m.group()) >= 2:
            out.append((m.start(), None))
    for m in re.finditer(r"\bmatches!\s*\(([^,()]*),([^()]*)\)", t):
        pat = m.group(2).split(" if ")[0]
        if re.fullmatch(r"[\s\d|.=]+", pat) and _oids(pat) >= 2:
            out.append((m.start(), None))
    return out


def decides(name):
    n = name.lower()
    return n in NAMES or bool(TOKENS & set(n.split("_"))) or any(p in n for p in PAIRS)


def _close(t, i, o, c):
    """-> the index after the bracket that closes the one at t[i] (`o`), or -1."""
    depth = 0
    while i < len(t):
        ch = t[i]
        if ch == o:
            depth += 1
        elif ch == c and not (c == ">" and t[i - 1] == "-"):
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return -1


def deciders(src):
    """-> [(name, line)] of the deciding functions `src` defines (test items are blanked by the lexer)."""
    t, out = src.joined("code"), []
    for m in _FN.finditer(t):
        name = m.group(1)
        if not decides(name):
            continue
        i = m.end()
        while i < len(t) and t[i].isspace():
            i += 1
        if i < len(t) and t[i] == "<":
            i = _close(t, i, "<", ">")
            while 0 <= i < len(t) and t[i].isspace():
                i += 1
        if i < 0 or i >= len(t) or t[i] != "(":
            continue
        j = _close(t, i, "(", ")")
        if j < 0:
            continue
        params, rest = t[i:j], t[j:j + 200]
        end = min([k for k in (rest.find("{"), rest.find(";"), rest.find(" where")) if k >= 0] or [len(rest)])
        if not re.match(r"\s*->\s*bool\s*$", rest[:end]) or "FunctionBuilder" in params:
            continue
        out.append((name, t.count("\n", 0, m.start()) + 1))
    return out


def deciders2(src):
    """-> [(name, line, body start, body end)] of the vocabulary-2 deciding functions `src` defines."""
    t, out = src.joined("code"), []
    for m in _FN.finditer(t):
        name = m.group(1)
        i = m.end()
        while i < len(t) and t[i].isspace():
            i += 1
        if i < len(t) and t[i] == "<":
            i = _close(t, i, "<", ">")
            while 0 <= i < len(t) and t[i].isspace():
                i += 1
        if i < 0 or i >= len(t) or t[i] != "(":
            continue
        j = _close(t, i, "(", ")")
        if j < 0:
            continue
        params, rest = t[i:j], t[j:j + 300]
        end = min([k for k in (rest.find("{"), rest.find(";"), rest.find(" where")) if k >= 0] or [len(rest)])
        r = re.match(r"\s*->\s*(.*?)\s*$", rest[:end], re.S)
        if not r or "FunctionBuilder" in params or not decides2(name, re.sub(r"\s+", "", r.group(1))):
            continue
        b = j + end
        if b >= len(t) or t[b] != "{":
            continue
        e = _close(t, b, "{", "}")
        out.append((name, t.count("\n", 0, m.start()) + 1, b, e if e > 0 else b))
    return out


def _enclosing(t, at):
    """The name of the last `fn` (or const / static) that starts before `at`."""
    name = "<item>"
    for m in re.finditer(r"\bfn\s+([A-Za-z_]\w*)|\b(?:const|static)\s+([A-Z_][A-Z0-9_]*)\s*:", t[:at]):
        name = m.group(1) or m.group(2)
    return name


def sites2(p, src):
    """-> {(path, name): (line, kind)}: vocabulary 2's sites in one file (RV outside the home, RW, RQ)."""
    out, t = {}, src.joined("code")
    decs = deciders2(src)
    if not is_home(p) and not in_any(p, _NOT_PLAN):
        for name, line, _, _ in decs:
            out.setdefault((p, name), (line, "calls+error"))
    if p.startswith(_RW_CRATE):
        for name, line, b, e in decs:
            if _WALK.search(t, b, e):
                out[(p, name)] = (line, "walk")
    in_eff = in_any(p, _EFFECTS_HOME) and not in_any(p, _GENERATED_HOME)
    spans = [(b, e, name) for name, _, b, e in decs]
    for at, label in _oid_lists(t):
        inside = next((n for b, e, n in spans if b <= at < e), None)
        if inside is None and not in_eff:
            continue
        key = inside or label or _enclosing(t, at)
        out.setdefault((p, key), (t.count("\n", 0, at) + 1, "list"))
    return out


def parse_debt(text):
    """-> ({(path, fn): (line, owner)}, [(line, problem)])."""
    rows, bad = {}, []
    for n, raw in enumerate(text.split("\n"), 1):
        s = raw.split("#", 1)[0].rstrip()
        if not s.strip():
            continue
        f = s.split("\t")
        if len(f) < 3 or not f[0].strip() or not f[1].strip() or not f[2].strip():
            bad.append((n, "a row is path<TAB>fn<TAB>owner lane<TAB>what it decides"))
            continue
        key = (f[0].strip(), f[1].strip())
        if key in rows:
            bad.append((n, f"duplicate row {key[0]} {key[1]}"))
        rows[key] = (n, f[2].strip())
    return rows, bad


def check_tree(ctx):
    out, hits = [], {}
    for p in walk_sources(ctx.root):
        if not SCOPE(p):
            continue
        t = ctx.read(p)
        if t is None or generated(t) or not (_NEEDS.search(t) or _NEEDS2.search(t)):
            continue
        src = Src(p, t)
        if not is_home(p):
            for name, line in deciders(src):
                hits.setdefault((p, name), line)
        for key, (line, _) in sites2(p, src).items():
            hits.setdefault(key, line)
    text, at = ctx.ledger(DEBT)
    rows, bad = parse_debt(text) if text is not None else ({}, [])
    out += [(*at(n), note) for n, note in bad]
    # A row follows its function: another landing may move a listed function to another file (a construct's own home
    # takes it) while this ledger still names the old path. A site with no row is then covered by a row of the same
    # function name whose own file no longer holds it, one row per site, so the count cannot grow: a note (re-key the
    # row), never a failure.
    gone = {}
    for (p, f), (n, owner) in sorted(rows.items(), key=lambda kv: kv[1][0]):
        if (p, f) not in hits:
            gone.setdefault(f, []).append((p, n))
    followed = set()
    for (p, f), n in sorted(hits.items()):
        if (p, f) not in rows:
            if gone.get(f):
                was, row = gone[f].pop(0)
                followed.add((was, f))
                ctx.notes.append(f"RV: {ledger_loc(at, row)}: `{f}` moved from {was} to {p}: re-key the row")
                continue
            out.append((p, n, f"`{f}` decides volatility, raise-freedom or row-order visibility outside the effects home, "
                              f"walks an expression again in lowering, or lists builtin oids; instead: {INSTEAD}"))
    stale = [f"{ledger_loc(at, n)}: {p} `{f}` ({owner}) decides nothing now: delete the row"
             for (p, f), (n, owner) in sorted(rows.items(), key=lambda kv: kv[1][0]) if (p, f) not in hits and (p, f) not in followed]
    ctx.notes += [f"RV: {s}" for s in stale]
    base_text = ctx.base_ledger(DEBT) if ctx.base_ledger else None
    if base_text is not None and not (VOCAB2 not in base_text and VOCAB2 in (text or "")):
        base, _ = parse_debt(base_text)
        removed = {bf for bp, bf in base if (bp, bf) not in rows}
        for (p, f), (n, owner) in sorted(rows.items(), key=lambda kv: kv[1][0]):
            if (p, f) not in base and f not in removed:
                out.append((*at(n), f"{p} `{f}` ({owner}) added: the list only shrinks (decide it in the effects home)"))
    return out


PLANTED_TOML = (G + "lowerv4/src/ops/scan/strategies/planted.rs",
                "fn calls_volatile(gx: &Gx<'_, '_>, k: usize) -> bool { gx.pipelines()[k].ops.is_empty() }\n")
CLEAN_TOML = (G + "lowerv4/src/ops/scan/strategies/clean.rs",
              "fn reverse(gx: &Gx<'_, '_>, k: usize) -> bool { crate::effects::visible::order_visible(gx, k).is_none() }\n"
              "fn raise_if(b: &mut FunctionBuilder<'_>, bad: Value) -> bool { true }\n")
# Vocabulary 2: a calm decider outside the home, a lowerv4 home decider that walks an expression again (RW), a builtin
# oid list in an effects home file (RQ), a list inside a decider outside the home.
MORE_PLANTED_TOML = (
    (G + "lowerv4/src/ops/scan/strategies/planted2.rs", "fn calm_arm(gx: &Gx<'_, '_>, k: usize) -> bool { gx.pipelines()[k].ops.is_empty() }\n"),
    (G + "lowerv4/src/effects/visible/planted3.rs",
     "pub(crate) fn quiet_arm(gx: &Gx<'_, '_>, n: Node<'_>) -> bool { !gravity_decompose::effects::of_expr(gx.cx(), n, &S).may_raise }\n"),
    (G + "decompose/src/effects/planted4.rs", "pub const LISTED: [u32; 3] = [1574, 2091, 2092];\n"),
    (G + "hosts/src/ops/agg/planted5.rs", "fn user_combine(f: u32) -> bool { matches!(f, 6293 | 6296) }\n"),
)
# A join side named `side` that is no Side class, a home decider that reads the node's Effects, kernel signatures listed
# outside the effects homes and outside any decider.
MORE_CLEAN_TOML = (
    (G + "abi/src/clean2.rs", "fn is_num_side(k: u8) -> bool { k == 1 }\n"),
    (G + "lowerv4/src/effects/visible/clean3.rs", "pub(crate) fn quiet_arm(e: Effects) -> bool { !e.may_raise }\n"),
    (G + "lowerv4/src/ops/window/clean4.rs", "const SIGS: [u32; 2] = [3100, 3101];\n"),
)
