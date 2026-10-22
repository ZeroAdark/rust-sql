"""RL: a function outside the lifetime home decides what a rescan keeps, rebuilds or resets (the user, 10-04: "a home for
option 2 and we can slowly migrate the other modules to use the new safeguards"). The plan-time half lives in
decompose/src/lifetime/ (the keep table: PG 18.6's ExecReScan answer per plan node, refusing by name with no rule), the
runtime half in lowerv4's lifetime/ (each word or object declares the scope that resets it). A decision is found two
ways, backed by the registry below:
- by name, in decompose (the plan reader): a `fn` whose name says it decides a rescan's keep (`kept`, `keepable`,
  `reexecuted`, `rescan`, `rewinds`, `resorts`, `rebuild(s)`, `purge(s)`), whatever it returns;
- by construct, anywhere in the gravity sources: a rescan answer (`OnRescan::`), a write to the kept set (`.kept.push`
  / `.extend` / `.insert`), a union-scoped word (`Scope::Union`), a reset of kept-table marks or of a hashed SubPlan's
  table (a call of `clear_marks` / `zero_marks`, a use of `m8_subhash_reset`, not their definitions), or the kept-run flag
  (`RERUN_KEPT`).
A function that asks the home (`lifetime::`, e.g. keep_rule / resolve) decides through it and is not flagged.

The registry, lint/LIFETIME-DEBT (a directory, one row per file), lists each such function not yet moved, by path and name, with its owner lane and kind;
a row covers that file's functions of that name. It also lists the shape guards, keeps and reset points the rescan audit
named that neither detector sees. A detected function with no row fails. On a landing (--base) a row the parent lacks fails
(the list only shrinks), unless it replaces a row of the same function name the landing removes (a certified move or
split). A row whose function its file no longer defines is a note, not a failure (delete it): another lane's landing that
moves or renames a listed function must not fail on this list, and a row for a function an entry queued ahead adds reads
as a note until it lands. The owner deletes a row when the decision moves into the home.

C4, the bound-at phase: in lowerv4, a parameter word placed outside lowerv4's lifetime/bound.rs (a WordPlan `key` /
`key_late` call: `.plan.key(` / `.plan.key_late(`), or a host-evaluated key cell (`computed_key(`) in a function that
declares no phase (`bound::declare` / `bound::index_key`), fails with no registry row: its evaluation phase would go
unchecked against PG's ExecReScan answer (decompose lifetime `Feed::pg`)."""
import re

from common import G, ledger_loc, Src, generated, home, in_any, in_src, walk_sources

ID = "RL"
TITLE = "a function outside the lifetime home decides what a rescan keeps, rebuilds or resets"
INSTEAD = "decompose lifetime::{keep_rule, resolve} (plan time), lowerv4 lifetime::{Scope, Lifetimes} (runtime); " \
          "not yet moved: a lint/LIFETIME-DEBT row"
GLOBAL = True
DEBT = G + "lint/LIFETIME-DEBT"
_HOME = home("lifetime")
SCOPE = in_src
NAME_SCOPE = G + "decompose/src/"

TOKENS = {"kept", "keepable", "reexecuted", "rescan", "rewinds", "resorts", "rebuild", "rebuilds", "purge", "purges"}
KINDS = {"keep", "reset", "scope", "guard"}
USE = re.compile(r"\bOnRescan::|\.kept\.(?:push|extend|insert)\b|\bScope::Union\b"
                 r"|(?<!fn )\b(?:clear_marks|zero_marks)\s*\(|(?<!fn )\bm8_subhash_reset\b|\bRERUN_KEPT\b")
_NEEDS = re.compile(r"kept|keepable|reexecuted|rescan|rewinds|resorts|rebuild|purge|OnRescan|Scope::Union|clear_marks"
                    r"|zero_marks|subhash_reset|RERUN_KEPT")
_FN = re.compile(r"\bfn\s+([A-Za-z_]\w*)")
BIND_SCOPE = G + "lowerv4/src/"
_BIND = re.compile(r"\.plan\.key(?:_late)?\s*\(")
_HOST_KEY = re.compile(r"(?<!fn )\bcomputed_key\s*\(")
_DECLARES = re.compile(r"\bbound::(?:declare|index_key)\s*\(")
_ASKS = re.compile(r"\blifetime::")


def is_home(p):
    return in_any(p, _HOME)


def named(name):
    return bool(TOKENS & set(name.lower().split("_")))


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


def _body_brace(t, j):
    """-> the offset from `j` (just past a fn's parameter list) of the `{` that opens its body, or -1 for a declaration
    (`;` first). A `;` inside brackets is part of the return type (`-> [Value; 2]`), not the declaration's end."""
    depth = 0
    for i in range(j, len(t)):
        ch = t[i]
        if ch == "[":
            depth += 1
        elif ch == "]":
            depth -= 1
        elif ch == "{" and depth <= 0:
            return i - j
        elif ch == ";" and depth <= 0:
            return -1
    return -1


def functions(src):
    """-> [(name, line, (body start, body end))] of the functions `src` defines with a body (test items are blanked by the
    lexer). A nested function's text is also in its parent's body: a construct is charged to the innermost one."""
    t, out = src.joined("code"), []
    for m in _FN.finditer(t):
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
        brace = _body_brace(t, j)
        if brace < 0:
            continue
        k = _close(t, j + brace, "{", "}")
        if k < 0:
            continue
        out.append((m.group(1), t.count("\n", 0, m.start()) + 1, (j + brace, k)))
    return out


def deciders(src):
    """-> [(name, line)] of the functions `src` defines that decide a keep, rebuild or reset (see the module doc); one
    that asks the home (`lifetime::`) decides through it."""
    t, fns = src.joined("code"), functions(src)
    out = set()
    if src.path.startswith(NAME_SCOPE):
        out |= {(n, line) for n, line, _ in fns if named(n)}
    for m in USE.finditer(t):
        inner = [(n, line, b - a) for n, line, (a, b) in fns if a <= m.start() < b]
        if inner:
            n, line, _ = min(inner, key=lambda x: x[2])
            out.add((n, line))
    asks = {n for n, _, (a, b) in fns if _ASKS.search(t, a, b)}
    return sorted((x for x in out if x[0] not in asks), key=lambda x: (x[1], x[0]))


def unbound(src):
    """-> [(name, line, what)] of the functions `src` defines that place a parameter word with no BoundAt (C4): a WordPlan
    `key` / `key_late` call outside lifetime/bound.rs, or a host key cell in a function that declares no phase."""
    t, fns = src.joined("code"), functions(src)
    out = set()

    def owner(i):
        inner = [(n, line, b - a) for n, line, (a, b) in fns if a <= i < b]
        return min(inner, key=lambda x: x[2])[:2] if inner else ("<file>", t.count("\n", 0, i) + 1)

    for m in _BIND.finditer(t):
        out.add((*owner(m.start()), "places a bind-time word (the WordPlan's key / key_late)"))
    for m in _HOST_KEY.finditer(t):
        n, line = owner(m.start())
        body = next(((a, b) for fn, ln, (a, b) in fns if fn == n and ln == line), None)
        if body is None or not _DECLARES.search(t, *body):
            out.add((n, line, "makes a host-evaluated key cell with no declared phase"))
    return sorted(out, key=lambda x: (x[1], x[0]))


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
        if t is not None and p.startswith(BIND_SCOPE) and not generated(t) and (_BIND.search(t) or _HOST_KEY.search(t)):
            for name, line, what in unbound(Src(p, t)):
                out.append((p, line, f"`{name}` {what} outside the bound-at home; instead: lowerv4 lifetime::bound::{{word, late, "
                                     f"index_key, declare}} (C4: its BoundAt is checked against PG's ExecReScan answer)"))
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
            out.append((p, n, f"`{f}` decides what a rescan keeps, rebuilds or resets outside the lifetime home; "
                              f"instead: {INSTEAD}"))
    stale = [f"{ledger_loc(at, n)}: {p} `{f}` ({owner}) is not in its file: delete the row once it is gone (an entry queued ahead may add it)"
             for (p, f), (n, owner, _) in sorted(rows.items(), key=lambda kv: kv[1][0])
             if (p, f) not in hits and f not in files.get(p, set())]
    ctx.notes += [f"RL: {s}" for s in stale]
    base_text = ctx.base_ledger(DEBT) if ctx.base_ledger else None
    if base_text is not None:
        base, _ = parse_debt(base_text)
        removed = {bf for bp, bf in base if (bp, bf) not in rows}
        for (p, f), (n, owner, _) in sorted(rows.items(), key=lambda kv: kv[1][0]):
            if (p, f) not in base and f not in removed:
                out.append((*at(n), f"{p} `{f}` ({owner}) added: the list only shrinks (decide it in the lifetime home)"))
    return out


MORE_PLANTED_TOML = ((G + "lowerv4/src/ops/limit/planted.rs",
                      "fn lim<'p>(px: &mut Px<'_, 'p>, e: Node<'p>) -> Place { px.plan.key(e) }\n"),
                     (G + "lowerv4/src/ops/scan/planted.rs",
                      "fn key(px: &mut Px<'_, '_>, v: Node<'_>, ids: &mut Vec<i32>) -> u16 { params_key::computed_key(1, v, ids) }\n"))
MORE_CLEAN_TOML = ((G + "lowerv4/src/ops/scan/clean.rs",
                    "fn key(px: &mut Px<'_, '_>, k: usize, v: Node<'_>, ids: &mut Vec<i32>) -> u16 {\n"
                    "    bound::declare(px, Feed::Tid, k, v, 1, BoundAt::Rescan);\n"
                    "    params_key::computed_key(1, v, ids)\n}\n"
                    "fn lim<'p>(px: &mut Px<'_, 'p>, k: usize, e: Node<'p>) -> Place { bound::word(px, Feed::Limit, k, e, 1) }\n"),
                   (G + "lowerv4/src/lifetime/bound.rs",
                    "pub(crate) fn word<'p>(px: &mut Px<'_, 'p>, v: Node<'p>) -> Place { px.plan.key(v) }\n"))
PLANTED_TOML = (G + "decompose/src/ops/sort/planted.rs",
                "fn kept_sort(b: &Builder<'_, '_>, q: usize) -> bool { b.closers.get(q).is_some() }\n"
                "fn mark(b: &mut Builder<'_, '_>, q: u16) { b.graph.kept.push(q); }\n")
CLEAN_TOML = (G + "decompose/src/ops/sort/clean.rs",
              "fn keeps_order(b: &Builder<'_, '_>) -> bool { true }\n"
              "fn rewound(b: &Builder<'_, '_>, q: usize) -> usize { b.graph.kept.len() + q }\n"
              "fn sorted(n: Node<'_>, at: &Rescan<'_, '_>) -> bool { matches!(keep_rule(n, at), Keep::Keep { .. }) }\n"
              "fn kept_root(b: &Builder<'_, '_>, q: usize) -> bool { matches!(lifetime::resolve(k, f, s), Kept::Keep { .. }) }\n")
