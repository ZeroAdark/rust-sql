"""RL2: a plain panic whose message names a limit (C6 (e); reports/BUG-RECURRENCE-20261011.md §3 C6, main
2026-10-11T08:09:47Z). Since D2933 a panic whose payload is not a PgError aborts the server (tcop's main_loop turns only a
PgError into an ERROR; any other payload goes to elog's abort_on_plain_panic). That is right for a broken invariant and
wrong for a LIMIT: an arena, share, slot, partition, run, key or size bound that user input reaches raises PostgreSQL's
error. The one home is `gravity_abi::limits::exceeded(format_args!(..))`: PgError 54000 (program_limit_exceeded) as the
panic payload, which a host's pad parks and the program's caller resumes, so the statement ends with an ERROR (program
arena.rs `exhausted` raises through it since D3933: an 80-way join aborted the server on 'arena exhausted').
RL2 reads every production Gravity source (`crates/gravity/<crate>/src/**`) and finds, charged to the innermost
function, a `panic!` / `assert!` / `assert_eq!` / `assert_ne!` / `unreachable!` / `.expect(..)` whose message literal names a
limit: exhausted, too many / large / long / deep / big / wide / small, overflow, capacity, exceed, out of space / room /
slots / memory, no room, ran out, at most, past MAX, sanity limit, full. A host's declared return
(`b.call_host(..).expect("returns the capacity")`) is not a panic of this kind.
A site not converted yet needs a row in lint/LIMIT-PANIC-DEBT (a directory, one row per file): path, function, owner
lane and why it stays; an unlisted one FAILS. On a landing (--base) a row the parent lacks fails (the list only shrinks),
unless it replaces a row of the same function the landing removes. A row whose site is gone is a note (delete it). A
bound no input reaches (the caller caps it, a type holds it, PostgreSQL's parser truncates it) or a panic only the
lowerer reaches (lowerv4's `catch` makes it a refusal, PostgreSQL runs the statement: no abort) is an exempt.py row
with the reason; the table applies to RL2 as to the per-file rules (`site_lines`, run.py `row_hits`)."""
import re

import exempt

from common import G, ledger_loc, Src, generated, in_src, walk_sources
from rl_lifetime import functions

ID = "RL2"
TITLE = "a plain panic whose message names a limit (it aborts the server since D2933)"
INSTEAD = "raise it as PostgreSQL's ERROR: gravity_abi::limits::exceeded(format_args!(..)) (54000); a bound no input " \
          "reaches: an exempt.py row saying why; not yet converted: a lint/LIMIT-PANIC-DEBT row"
GLOBAL = True
DEBT = G + "lint/LIMIT-PANIC-DEBT"

_SITE = re.compile(r"\b(?:panic|assert|assert_eq|assert_ne|unreachable)!\s*[(\[{]|\.expect\s*\(")
_LIMIT = re.compile(r"exhaust|too (?:many|large|long|deep|big|wide|small)|overflow|capacit|exceed"
                    r"|out of (?:space|room|slots?|memory)|no room|ran out|run out|\bat most\b|\bpast MAX|sanity limit"
                    r"|\bfull\b", re.I)
_NEEDS = re.compile(r"exhaust|too |overflow|capacit|exceed|out of|no room|ran out|run out|at most|past MAX|sanity|full",
                    re.I)
_STR = re.compile(r'b?"((?:\\.|[^"\\])*)"')
_HOSTRET = re.compile(r"call_host\s*\(")
_CONST_CTX = re.compile(r"\bconst\s+[A-Z_][A-Z0-9_]*\s*:[^=]*=\s*$|\bconst\s+_\s*:\s*\(\)\s*=\s*$")
_FN_NAME = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")


def _close(t, i):
    """t[i] is an opening bracket: the index after its match (string literals skipped), or len(t)."""
    depth, j, n = 0, i, len(t)
    while j < n:
        c = t[j]
        if c == '"':
            j += 1
            while j < n and t[j] != '"':
                j += 2 if t[j] == "\\" else 1
        elif c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
            if depth == 0:
                return j + 1
        j += 1
    return n


def _strip_keep_strings(text, code_lines):
    """The source with comments and #[cfg(test)] items blanked (the lexer's code lines) but string literals kept: the
    lexer empties strings, so each code line is re-filled from the source wherever the lexer kept a `""`."""
    out = []
    for raw, code in zip(text.split("\n"), code_lines):
        if not code.strip():
            out.append(code)
            continue
        cut = raw.find("//")
        while cut >= 0 and raw[:cut].count('"') % 2 == 1:   # a `//` inside a string literal
            cut = raw.find("//", cut + 2)
        out.append(raw if cut < 0 else raw[:cut])
    return "\n".join(out)


def sites(src):
    """-> {(function, line): message} of the plain panics of `src` whose message names a limit."""
    t = _strip_keep_strings(src.text, src.code)
    code = src.joined("code")
    # functions() spans are offsets in the lexer's code text; `t` keeps the strings, so spans are compared by line
    fns = [(n, code.count("\n", 0, a) + 1, code.count("\n", 0, b) + 1) for n, _, (a, b) in functions(src)]
    out = {}
    for m in _SITE.finditer(t):
        end = _close(t, m.end() - 1)
        msg = " ".join(_STR.findall(t[m.end() - 1:end]))
        if not _LIMIT.search(msg):
            continue
        if m.group().startswith(".expect") and _HOSTRET.search(t[max(0, t.rfind("\n", 0, m.start()) - 200):m.start()]):
            continue
        line = t.count("\n", 0, m.start()) + 1
        head = t[t.rfind("\n", 0, m.start()) + 1:m.start()]
        if _CONST_CTX.search(head):
            continue                      # a const item's assert: evaluated by the compiler, no input reaches it
        inner = [(n, b - a) for n, a, b in fns if a <= line <= b]
        if inner:
            name = min(inner, key=lambda x: x[1])[0]
        else:                             # a fn functions() misses (a `[T; N]` return type): the nearest `fn` above
            fm = list(_FN_NAME.finditer(t, 0, m.start()))
            name = fm[-1].group(1) if fm else "<item>"
        out.setdefault((name, line), msg[:120])
    return out


def site_lines(src):
    """-> the lines of `src`'s limit panics (the exempt table's selftest reads a row's test through it)."""
    return sorted({line for _, line in sites(src)})


def debt_file(path, name):
    """A row's file name: <path below crates/gravity, '/' as '.'>@<function>."""
    return path[len(G):].replace("/", ".") + "@" + name


def parse_debt(text):
    """-> ({(path, function): (line, owner)}, [(line, problem)])."""
    rows, bad = {}, []
    for n, raw in enumerate(text.split("\n"), 1):
        s = raw.split("#", 1)[0].rstrip()
        if not s.strip():
            continue
        f = s.split("\t")
        if len(f) < 4 or not all(x.strip() for x in f[:4]):
            bad.append((n, "a row is path<TAB>function<TAB>owner lane<TAB>why it stays"))
            continue
        key = (f[0].strip(), f[1].strip())
        if key in rows:
            bad.append((n, f"duplicate row {key[0]} {key[1]}"))
        rows[key] = (n, f[2].strip())
    return rows, bad


def scan(ctx):
    """-> {(path, function): (line, message)} over the production Gravity sources."""
    hits = {}
    for p in walk_sources(ctx.root):
        if not in_src(p):
            continue
        t = ctx.read(p)
        if t is None or generated(t) or not _NEEDS.search(t):
            continue
        src = Src(p, t)
        found = sites(src)
        kept = set(exempt.excused(ID, src, sorted({line for _, line in found}), ctx.exempt_used))
        for (name, line), msg in sorted(found.items()):
            if line in kept:
                hits.setdefault((p, name), (line, msg))
    return hits


def check_tree(ctx):
    out = []
    text, at = ctx.ledger(DEBT)
    rows, bad = parse_debt(text) if text is not None else ({}, [])
    out += [(*at(n), note) for n, note in bad]
    hits = scan(ctx)
    for (p, name), (n, msg) in sorted(hits.items()):
        if (p, name) not in rows:
            out.append((p, n, f"`{name}` panics on a limit ('{msg}'): a plain panic aborts the server; instead: {INSTEAD}"))
    ctx.notes += [f"RL2: {ledger_loc(at, n)}: {p} `{name}` ({owner}) holds no limit panic: delete the row"
                  for (p, name), (n, owner) in sorted(rows.items(), key=lambda kv: kv[1][0]) if (p, name) not in hits]
    if rows:
        ctx.notes.append(f"RL2: LIMIT-PANIC-DEBT {len(rows)} rows")
    base_text = ctx.base_ledger(DEBT) if ctx.base_ledger else None
    if base_text is not None:
        base, _ = parse_debt(base_text)
        removed = {bn for bp, bn in base if (bp, bn) not in rows}
        for (p, name), (n, owner) in sorted(rows.items(), key=lambda kv: kv[1][0]):
            if (p, name) not in base and name not in removed:
                out.append((*at(n), f"{p} `{name}` ({owner}) added: the list only shrinks (raise a PgError instead)"))
    return out


# the planted failure panics plainly on a share that input sizes; the clean one raises PostgreSQL's error
PLANTED_TOML = (G + "abi/src/planted.rs",
                "fn open(share: usize, row: usize) {\n    assert!(share > row, \"a share too small for one row\");\n}\n")
CLEAN_TOML = (G + "abi/src/clean.rs",
              "fn open(share: usize, row: usize) {\n    if share <= row {\n        crate::limits::exceeded(format_args!(\"a share "
              "too small for one row: {share} bytes for {row}\"));\n    }\n    // not assert!(share > row, \"too small\") in a "
              "comment\n}\n")
MORE_PLANTED_TOML = [
    (G + "program/src/planted.rs",            # an expect on an exhausted pool
     "fn take(p: &mut Pool) -> u32 {\n    p.next().expect(\"the pool is exhausted\")\n}\n"),
]
MORE_CLEAN_TOML = [
    (G + "lowerv4/src/clean.rs",              # a host's declared return is not a limit panic
     "fn grow(b: &mut B, h: H) -> V {\n    b.call_host(h, &[ctx, total], Some(Type::I64)).expect(\"out_grow returns the new "
     "capacity\")\n}\n"),
]
