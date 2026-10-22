"""RR: every road on by default can be switched off, says when it fired, and has a registry row. A road is a lowering
strategy declared in a `strategies!` block (lowerv4 face/strategy.rs). Its class comes from lowerv4 road_off.rs:
`DEFAULT_OFF`, `PLANNED`, `CORE` and `FALLBACK` list theirs; any other road is on by default. A road on by default that
nothing switches off, or that leaves no trace when it fires, cannot be judged against its own off arm, and a narrow
road then stays on for the one statement shape it was written for.

Checked: every road on by default, and every `CORE` road. Each must have
- reader: its constant or its name is read through a reader that honours `gravity_road_off` (not checked for `CORE`
  roads, which have no off switch). Readers, found statically:
  - `<env>.applies(C)` (frame/env.rs `Env::applies`), `<x>.strategies.has(C)` (`Px::strategies`, applies.rs
    `applicable`), `strategy::off().has(C)`, `source::on(Variant::Strategy(C))`;
  - `E::V.on()` where `hook_variants!` declares `V = Strategy(C)` in `enum E`;
  - a function with a `Strategy` (or hook enum) parameter that reads the parameter through one of the above, called
    with `C` (or `E::V`) in the same file or by its module path (a `use .. as` alias resolved);
  - a binding in one function: `let x = ..C..;`, `for x in [..C..] { .. }` or `[..C..].iter().any(|x| ..)`, with `x`
    read through one of the above (or such a function) in the rest of the function, the loop body or the closure;
    or a closure `let f = |.., r| ..;` that reads its parameter so, called `f(.., C, ..)` in that function;
  - an expression extension row `(Some(C), ..)` in `ops::ext_rows` or in a table passed to `face::ext::exts_in`;
  - decompose and hosts read their roads by name: `road_on(N)`, `default_on_road_on(N)` or `road_named_off(N)`, with
    `N` the name or a `&str` constant holding it.
  `need_strategy`, `ops::need`, `ops::strategy` and a raw registry row do not honour the off set;
- fire: a row in lowerv4/registry/road-fires.tsv (road, fire) whose fire is fired (`fires::fire`, `fires::fire_shared`)
  somewhere in the sources;
- side: a row in lowerv4/registry/registry-side.tsv with a trigger and a motivator (not empty, not "-").
`PLANNED`, `DEFAULT_OFF` and `FALLBACK` roads are not checked: none is an optimization on by default.

The debt, lint/ROAD-DEBT (a directory, one row per file named by the road), lists the roads that still fail a check,
with the checks they fail. A checked road failing a check its row does not name fails. On a landing (--base) a road
the parent's debt lacks fails, and so does a row naming a check its parent row did not: the debt only shrinks, so a
new road passes every check. A row naming a check its road passes is a note (drop the check), as is a row, a fire row
or a sidecar row whose road no longer exists. A fire row of an existing road whose fire nothing fires fails (the map
would vouch for a signal that is gone)."""
import os
import re

import common
from common import G, L, Src, generated, ledger_loc, walk_sources

ID = "RR"
TITLE = "R-ROAD: a road on by default without an off-set reader, a mapped fire or a registry row"
INSTEAD = "read the road through Env::applies / px.strategies / strategy::off() / a hook variant's on(), add its fire " \
          "to lowerv4/registry/road-fires.tsv and its trigger + motivator to lowerv4/registry/registry-side.tsv; " \
          "a road with no other implementation goes on road_off.rs CORE"
GLOBAL = True
DEBT = G + "lint/ROAD-DEBT"
FIRES = G + "lowerv4/registry/road-fires.tsv"
SIDE = G + "lowerv4/registry/registry-side.tsv"
CHECKS = ("reader", "fire", "side")
CLASSES = ("DEFAULT_OFF", "PLANNED", "CORE", "FALLBACK")
SIDE_COLS = ("road", "d_id", "trigger", "constants", "motivator", "evidence")

_P = r"(?:[A-Za-z_]\w*::)*"
_DECL = re.compile(r'([A-Z][A-Z0-9_]*)\s*=\s*(\d+)\s*,\s*"([a-z0-9_.]+)"\s*,\s*[A-Za-z_]\w*\s*;')
_STRATEGIES = re.compile(r"\bstrategies!\s*\{")
_HOOKS = re.compile(r"\bhook_variants!\s*\{")
_ENUM = re.compile(r"\benum\s+([A-Z]\w*)\s*\{")
_ROW = re.compile(r"\b([A-Z]\w*)\s*=\s*Strategy\(\s*([A-Z][A-Z0-9_]*)\s*\)")
_STR_CONST = re.compile(r'\bconst\s+([A-Z_][A-Z0-9_]*)\s*:\s*&(?:\'static\s+)?str\s*=\s*"([a-z0-9_.:\-]+)"\s*;')
_FN = re.compile(r"\bfn\s+([a-z_]\w*)\s*(?:<[^>{;]*>)?\s*\(")
_FIRE = re.compile(r"\bfire(?:_shared)?\s*\(")
_LIT = re.compile(r'"([a-z0-9_.:\-]+)"')
_NAME_READERS = r"\b(?:road_on|default_on_road_on|road_named_off)\s*\(\s*"


def _close(t, i, o="{", c="}"):
    """The index just past the bracket that closes the one at `i`."""
    depth = 0
    for j in range(i, len(t)):
        if t[j] == o:
            depth += 1
        elif t[j] == c:
            depth -= 1
            if depth == 0:
                return j + 1
    return len(t)


def _plain(text, live=None):
    """`text` with comments blanked (line breaks kept), string literals kept and #[cfg(test)] items' lines blanked: the
    code view of `common.lex` (`live`, when the caller has it) blanks the strings a road's name and a fire's name live in."""
    def repl(m):
        t = m.group()
        return "\n" * t.count("\n") if t[0] == "/" else t
    out = common._TOK.sub(repl, text)
    if "#[cfg(test)]" in text:
        live = live if live is not None else common.lex(text)[0]
        whole = common.lex(text.replace("#[cfg(test)]", "#[cfg(nottest)]"))[0]
        lines = out.split("\n")
        for n, (a, b) in enumerate(zip(live, whole)):
            if b.strip() and not a.strip() and n < len(lines):
                lines[n] = ""
        out = "\n".join(lines)
    return out


def _token(c):
    """A pattern for constant `c` as an argument (any path in front)."""
    return _P + re.escape(c) + r"\b"


def _direct(tok):
    """The direct off-set reads of `tok` (a pattern for a constant, a hook variant or a bound name)."""
    return [re.compile(r"\.\s*applies\(\s*" + tok + r"\s*\)"),
            re.compile(r"\bstrategies\s*\.\s*has\(\s*" + tok + r"\s*\)"),
            re.compile(r"\boff\(\)\s*\.\s*has\(\s*" + tok + r"\s*\)"),
            re.compile(r"\bon\(\s*" + _P + r"Variant::Strategy\(\s*" + tok + r"\s*\)\s*\)")]


def _reads(text, tok, variant=False):
    """`text` reads `tok` through a direct off-set reader (`variant`: `tok` is a hook variant value, read by `.on()`)."""
    if variant and re.search(tok + r"\s*\.\s*on\(\)", text):
        return True
    return any(rx.search(text) for rx in _direct(tok))


def _fn_bodies(t):
    """-> [(name, params, body start, body end)] of every fn with a body in code text `t`."""
    out = []
    for m in _FN.finditer(t):
        a = m.end() - 1
        b = _close(t, a, "(", ")")
        brace = t.find("{", b)
        semi = t.find(";", b)
        if brace < 0 or (0 <= semi < brace):
            continue
        out.append((m.group(1), t[a + 1:b - 1], brace, _close(t, brace)))
    return out


class Tree:
    """What RR reads of the tree: the roads, their classes, the off-set reads, the fires and the registry rows."""

    def __init__(self, ctx):
        self.ctx = ctx
        self.roads = {}             # name -> (const, path, line)
        self.cls = {}               # const -> class
        self.hooks = {}             # const -> {(enum, variant)}
        self.str_consts = {}        # value -> {const names}
        self.const_values = {}      # const name -> {values}
        self.fired = set()
        self.road_off = None
        lowering, others = [], []
        for p in walk_sources(ctx.root):
            t = ctx.read(p)
            if t is None or generated(t):
                continue
            (lowering if any(p.startswith(G + c + "/src/") for c in common.LOWERING) else others).append((p, t))
        self.lowering = [(p, Src(p, t)) for p, t in lowering]
        live = {p: s.code for p, s in self.lowering}
        self.plain = {}
        for p, t in lowering + others:
            for m in _STR_CONST.finditer(t):
                self.str_consts.setdefault(m.group(2), set()).add(m.group(1))
                self.const_values.setdefault(m.group(1), set()).add(m.group(2))
            if any(k in t for k in ("fires::", "use fires", "road_on", "road_named_off", "strategies!")):
                self.plain[p] = _plain(t, live.get(p))
        for p, s in self.lowering:
            pt = self.plain.get(p)
            if pt is not None and "strategies!" in pt:
                for m in _STRATEGIES.finditer(pt):
                    a = m.end() - 1
                    body = pt[a:_close(pt, a)]
                    for d in _DECL.finditer(body):
                        line = pt.count("\n", 0, a + d.start()) + 1
                        self.roads[d.group(3)] = (d.group(1), p, line)
            code = s.joined("code")
            if "hook_variants!" in code:
                for m in _HOOKS.finditer(code):
                    a = m.end() - 1
                    body = code[a:_close(code, a)]
                    for e in _ENUM.finditer(body):
                        eb = body[e.end() - 1:_close(body, e.end() - 1)]
                        for r in _ROW.finditer(eb):
                            self.hooks.setdefault(r.group(2), set()).add((e.group(1), r.group(1)))
        for p in L("road_off.rs"):
            t = ctx.read(p)
            if t is not None:
                self.road_off = p
                code = Src(p, t).joined("code")
                for k in CLASSES:
                    m = re.search(r"\bconst\s+" + k + r"\b[^=]*=\s*&\[(.*?)\];", code, re.S)
                    for c in re.findall(r"\b([A-Z][A-Z0-9_]*)\b", m.group(1)) if m else []:
                        self.cls.setdefault(c, k)
        for p, pt in self.plain.items():
            if "fire" not in pt or ("fires::" not in pt and "use fires" not in pt):
                continue
            for m in _FIRE.finditer(pt):
                if pt[max(0, m.start() - 3):m.start()] == "fn ":
                    continue
                arg = pt[m.end():_close(pt, m.end() - 1, "(", ")") - 1]
                self.fired.update(_LIT.findall(arg))
                for c in re.findall(r"\b([A-Z][A-Z0-9_]+)\b", arg):
                    self.fired.update(self.const_values.get(c, ()))
        self._index()

    def _index(self):
        """The off-set reads, indexed once: per file its identifiers, hook variants, `use .. as` aliases and fns; the
        wrapper functions; the extension rows; the names the name readers read."""
        enum_names = {e for vs in self.hooks.values() for e, _ in vs}
        ptype = re.compile(r"\b([a-z_]\w*)\s*:\s*(?:&\s*)?" + _P + r"(Strategy|" + "|".join(sorted(enum_names) or ["\x00"]) + r")\b")
        hook_tokens = {f"{e}::{v}" for vs in self.hooks.values() for e, v in vs}
        self.wrappers = {}          # fn name -> {the paths defining it, "::<module>"}
        self.code, self.fns, self.words, self.alias = {}, {}, {}, {}
        for p, s in self.lowering:
            t = s.joined("code")
            self.code[p] = t
            self.fns[p] = _fn_bodies(t)
            self.words[p] = set(_WORD.findall(t)) | (set(_PAIR.findall(t)) & hook_tokens)
            self.alias[p] = {al: n for u in _USE.findall(t) for n, al in _AS.findall(u)}
            for name, params, a, b in self.fns[p]:
                for pm in ptype.finditer(params):
                    if _reads(t[a:b], r"\b" + re.escape(pm.group(1)) + r"\b", variant=pm.group(2) != "Strategy"):
                        stem = os.path.basename(p)[:-3]
                        if stem == "mod":
                            stem = os.path.basename(os.path.dirname(p))
                        self.wrappers.setdefault(name, set()).update({p, "::" + stem})
        self.ext = ""               # expression extension rows: `ops::ext_rows`, and tables handed to `exts_in`
        for p, t in self.code.items():
            for name in set(re.findall(r"\bexts_in\(\s*&?\s*" + _P + r"([A-Za-z_]\w*)", t)) | {"ext_rows"}:
                for m in re.finditer(r"\bfn\s+" + name + r"\b[^{;]*\{", t):
                    self.ext += t[m.end() - 1:_close(t, m.end() - 1)] + "\n"
                for m in re.finditer(r"\b(?:static|const)\s+" + name + r"\b[^=;]*=", t):
                    self.ext += t[m.end():t.find(";", m.end())] + "\n"
        self.calls = {}
        self._call_rx = re.compile(r"(?:\b([A-Za-z_]\w*)::)?\b(" + "|".join(sorted(map(re.escape, self.wrappers)) or ["\x00"]) + r")\s*\(")
        self.named = set()          # what `road_on` / `default_on_road_on` / `road_named_off` read: "name" or CONST
        for pt in self.plain.values():
            self.named.update(m.group(1) for m in _NAMED.finditer(pt))

    def reader(self, name):
        """-> True when road `name` is read through a reader that honours the off set (module doc)."""
        const = self.roads[name][0]
        hooks = sorted(self.hooks.get(const, ()))
        if '"' + name + '"' in self.named or self.str_consts.get(name, set()) & self.named:
            return True
        if re.search(r"\bSome\(\s*" + _token(const) + r"\s*\)", self.ext):
            return True
        toks = [(_token(const), False)] + [(r"\b" + re.escape(e) + "::" + re.escape(v) + r"\b", True) for e, v in hooks]
        word = re.compile("|".join([r"\b" + re.escape(const) + r"\b"] +
                                   [r"\b" + re.escape(e) + "::" + re.escape(v) + r"\b" for e, v in hooks]))
        keys = {const} | {f"{e}::{v}" for e, v in hooks}
        for p, t in self.code.items():
            if not keys & self.words[p]:
                continue
            if any(_reads(t, tok, variant) for tok, variant in toks) or self._wrapped(p, t, word) or self._bound(p, word):
                return True
        return False

    def _wrapped(self, p, t, word):
        """A call in `t` of an off-set-reading wrapper with the road in its arguments, in the wrapper's file or by its
        module (a `use .. as` alias resolved)."""
        if t is self.code[p]:
            if p not in self.calls:
                self.calls[p] = self._calls(p, t)
            calls = self.calls[p]
        else:
            calls = self._calls(p, t)
        return any(word.search(args) for args in calls)

    def _calls(self, p, t):
        """The argument texts of the wrapper calls in `t` that reach a wrapper (its file, or its module by path)."""
        out = []
        if not self.wrappers:
            return out
        for m in self._call_rx.finditer(t):
            if t[max(0, m.start() - 3):m.start()] == "fn ":
                continue
            where = self.wrappers[m.group(2)]
            q = m.group(1) and self.alias[p].get(m.group(1), m.group(1))
            if p in where or (q and "::" + q in where):
                out.append(t[m.end():_close(t, m.end() - 1, "(", ")") - 1])
        return out

    def _bound(self, p, word):
        """The road bound to a name in one function, the name read through a direct reader, or handed to a closure of
        that function that reads its parameter so (module doc)."""
        t = self.code[p]
        for _, _, a, b in self.fns[p]:
            body = t[a:b]
            if not word.search(body):
                continue
            for m in _LET.finditer(body):
                end = _stmt_end(body, m.end())
                if word.search(body[m.end():end]) and self._pat_read(p, m.group(1), body[end:]):
                    return True
            for m in _CLOSURE.finditer(body):
                end = _stmt_end(body, m.end())
                if not self._pat_read(p, m.group(2), body[m.end():end]):
                    continue
                for c in re.finditer(r"\b" + re.escape(m.group(1)) + r"\s*\(", body[end:]):
                    i = end + c.end() - 1
                    if word.search(body[i:_close(body, i, "(", ")")]):
                        return True
            for m in re.finditer(r"\bfor\s+(.+?)\s+in\s+", body):
                brace = body.find("{", m.end())
                while brace >= 0 and body[m.end():brace].count("[") != body[m.end():brace].count("]"):
                    brace = body.find("{", brace + 1)
                if brace >= 0 and word.search(body[m.end():brace]) and self._pat_read(p, m.group(1), body[brace:_close(body, brace)]):
                    return True
            for m in re.finditer(r"\[", body):
                end = _close(body, m.start(), "[", "]")
                if not word.search(body[m.start():end]):
                    continue
                chain = _CHAIN.match(body, end)
                if chain:
                    call = body.rfind("(", 0, chain.start(1))
                    if self._pat_read(p, chain.group(1), body[call:_close(body, call, "(", ")")]):
                        return True
        return False

    def _pat_read(self, p, pat, text):
        """A name bound by pattern (or closure parameter list) `pat` is read in `text` through a direct reader or a
        wrapper."""
        for x in re.findall(r"\b([a-z_]\w*)\b", pat):
            if x not in ("mut", "ref", "_", "move") and (_reads(text, r"\b" + re.escape(x) + r"\b", variant=True)
                                                        or self._wrapped(p, text, re.compile(r"\b" + re.escape(x) + r"\b"))):
                return True
        return False


_WORD = re.compile(r"\b[A-Z][A-Z0-9_]+\b")
_PAIR = re.compile(r"\b[A-Z]\w*::[A-Z]\w*\b")
_USE = re.compile(r"\buse\s+[^;]*;")
_AS = re.compile(r"\b([A-Za-z_]\w*)\s+as\s+([A-Za-z_]\w*)")
_NAMED = re.compile(_NAME_READERS + _P + r'("[a-z0-9_.]+"|[A-Z][A-Z0-9_]*)\s*\)')
_LET = re.compile(r"\blet\s+(?:mut\s+)?([^=;|]+?)(?::[^=;]*)?=(?!=)")
_CLOSURE = re.compile(r"\blet\s+([a-z_]\w*)\s*(?::[^=;]*)?=\s*(?:move\s+)?\|([^|]*)\|")
_CHAIN = re.compile(r"\s*\.\s*(?:into_iter|iter)\(\)(?:\s*\.\s*(?:copied|cloned)\(\))?\s*\.\s*\w+\(\s*(?:move\s+)?\|([^|]*)\|")


def _stmt_end(body, i):
    """The index of the `;` that ends the statement running at `i` (brackets balanced), or the body's end."""
    depth = 0
    while i < len(body) and not (body[i] == ";" and depth == 0):
        depth += body[i] in "([{"
        depth -= body[i] in ")]}"
        i += 1
    return i


def _rows(text, ncols, what):
    """A TSV registry's rows -> ([(line, cols)], [(line, problem)]): '#' lines and the header (its first cell 'road')
    skipped; a row with fewer than `ncols` columns is a problem."""
    rows, bad = [], []
    for n, raw in enumerate(text.split("\n"), 1):
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        f = [x.strip() for x in raw.split("\t")]
        if f[0] == "road":
            continue
        if len(f) < ncols or not f[0]:
            bad.append((n, f"{what}: a row is {ncols} tab-separated columns, the road first"))
            continue
        rows.append((n, f))
    return rows, bad


def parse_debt(text):
    """-> ({road: (line, {checks})}, [(line, problem)])."""
    rows, bad = {}, []
    for n, raw in enumerate(text.split("\n"), 1):
        s = raw.split("#", 1)[0].rstrip()
        if not s.strip():
            continue
        f = [x.strip() for x in s.split("\t")]
        checks = {c.strip() for c in f[1].split(",")} if len(f) > 1 else set()
        if len(f) < 2 or not f[0] or not checks or not checks <= set(CHECKS):
            bad.append((n, f"a row is road<TAB>the checks it still fails, a comma list of {', '.join(CHECKS)}"))
            continue
        if f[0] in rows:
            bad.append((n, f"duplicate row {f[0]}"))
        rows[f[0]] = (n, checks)
    return rows, bad


def check_tree(ctx):
    tree = Tree(ctx)
    out = []
    if tree.road_off is None:
        return out
    if not tree.roads:
        return [(tree.road_off, 1, "RR read no `strategies!` declaration: its parse no longer matches the macro")]
    checked = {n: tree.cls.get(c) for n, (c, _, _) in tree.roads.items() if tree.cls.get(c) in (None, "CORE")}
    # the maps
    fires, side = {}, {}
    t = ctx.read(FIRES)
    rows, bad = _rows(t, 2, "road-fires.tsv") if t is not None else ([], [])
    out += [(FIRES, n, note) for n, note in bad]
    seen = set()
    for n, (road, fire) in [(n, f[:2]) for n, f in rows]:
        if (road, fire) in seen:
            out.append((FIRES, n, f"duplicate row {road} {fire}"))
        seen.add((road, fire))
        if road not in tree.roads:
            ctx.notes.append(f"RR: {FIRES}:{n}: road {road} is declared nowhere: delete the row")
        elif fire not in tree.fired:
            out.append((FIRES, n, f"road {road}'s fire {fire} is fired nowhere: map the road to the name its site fires, "
                                  f"or delete the row"))
        else:
            fires.setdefault(road, set()).add(fire)
    t = ctx.read(SIDE)
    rows, bad = _rows(t, len(SIDE_COLS), "registry-side.tsv") if t is not None else ([], [])
    out += [(SIDE, n, note) for n, note in bad]
    for n, f in rows:
        if f[0] in side:
            out.append((SIDE, n, f"duplicate row {f[0]}"))
        side[f[0]] = (n, f)
        if f[0] not in tree.roads:
            ctx.notes.append(f"RR: {SIDE}:{n}: road {f[0]} is declared nowhere: delete the row")
    # the debt
    text, at = ctx.ledger(DEBT)
    debt, bad = parse_debt(text) if text is not None else ({}, [])
    out += [(*at(n), note) for n, note in bad]
    for road, (const, path, line) in sorted(tree.roads.items(), key=lambda kv: kv[1][1:]):
        if road not in checked:
            continue
        failing = []
        if checked[road] != "CORE" and not tree.reader(road):
            failing.append(("reader", "no reader that honours gravity_road_off reads it (need_strategy, ops::need and "
                                      "a call by name do not)"))
        if not fires.get(road):
            failing.append(("fire", f"no row of {FIRES} maps it to a fire its code fires"))
        s = side.get(road)
        if s is None or any(s[1][SIDE_COLS.index(k)] in ("", "-") for k in ("trigger", "motivator")):
            failing.append(("side", f"no row of {SIDE} with a trigger and a motivator"))
        owed = debt.get(road, (0, set()))[1]
        for c, why in failing:
            if c not in owed:
                out.append((path, line, f"road {road} ({const}{', core' if checked[road] else ''}): {why}; "
                                        f"instead: {INSTEAD}"))
        if road in debt:
            gone = owed - {c for c, _ in failing}
            if gone:
                ctx.notes.append(f"RR: {ledger_loc(at, debt[road][0])}: road {road} passes {', '.join(sorted(gone))} "
                                 f"now: drop it from the row")
    for road, (n, _) in sorted(debt.items(), key=lambda kv: kv[1][0]):
        if road not in checked:
            ctx.notes.append(f"RR: {ledger_loc(at, n)}: road {road} is {'declared nowhere' if road not in tree.roads else 'not on by default'}"
                             f": delete the row")
    base_text = ctx.base_ledger(DEBT) if ctx.base_ledger else None
    if base_text is not None:
        base, _ = parse_debt(base_text)
        for road, (n, checks) in sorted(debt.items(), key=lambda kv: kv[1][0]):
            if road not in base:
                out.append((*at(n), f"road {road} added to the debt: the debt only shrinks, so a new road passes every check"))
            elif checks - base[road][1]:
                out.append((*at(n), f"road {road}'s row gained {', '.join(sorted(checks - base[road][1]))}: the debt only shrinks"))
    return out


# ---------------------------------------------------------------------------------------------------------------------
# Self-test cases (run.py --selftest): a tree is a lowerv4 crate with a strategies! block, road_off.rs, the two registry
# files and the debt.

_OFF = (G + "lowerv4/src/road_off.rs",
        "use crate::face::strategy;\npub const DEFAULT_OFF: &[strategy::Strategy] = &[strategy::KEPT_OFF];\n"
        "pub const PLANNED: &[strategy::Strategy] = &[strategy::LATER];\n"
        "pub const CORE: &[strategy::Strategy] = &[strategy::ONLY_FORM];\n"
        "pub const FALLBACK: &[strategy::Strategy] = &[];\n")
_DECLS = ("crate::strategies! {\n    /// a road\n    FAST_PATH = 0, \"fast.path\", Scan;\n    KEPT_OFF = 1, \"kept.off\", Scan;\n"
          "    LATER = 2, \"later\", Scan;\n    ONLY_FORM = 3, \"only.form\", Scan;\n}\n")
_FIRED = "fn fired() { fires::fire(\"fast.path\"); fires::fire(\"only.form\"); }\n"
_FIRES_OK = (FIRES, "road\tfire\nfast.path\tfast.path\nonly.form\tonly.form\n")
_SIDE_HEAD = "road\td_id\ttrigger\tconstants\tmotivator\tevidence\n"
_SIDE_OK = (SIDE, _SIDE_HEAD + "fast.path\tD1\ta filter over a scan\t-\tq1\t-\nonly.form\tD2\tevery scan\t-\tq2\t-\n")


def _tree(reader, *more, fires=_FIRES_OK, side=_SIDE_OK, debt=None, base=None, want=None):
    files = [_OFF, (G + "lowerv4/src/ops/scan/fast.rs", _DECLS + _FIRED + reader), fires, side, *more]
    if debt is not None:
        files += [(DEBT + "/README", "# rows\n")] + [(DEBT + "/" + r.split("\t")[0], r + "\n") for r in debt]
    case = {"files": files}
    if base is not None:
        case["base"] = {DEBT: "".join(r + "\n" for r in base)}
    if want:
        case["want"] = want
    return case


_APPLIES = "fn lower(env: &Env) -> bool { env.applies(FAST_PATH) }\n"
PLANTED_TOML = _tree("fn lower(cx: &Cx) -> &'static Impl { need_strategy::<Impl>(cx, FAST_PATH) }\n", want="no reader")
MORE_PLANTED_TOML = (
    _tree(_APPLIES, fires=(FIRES, "road\tfire\nonly.form\tonly.form\n"), want="no row of"),
    _tree(_APPLIES, fires=(FIRES, "road\tfire\nfast.path\tfast.path.gone\nonly.form\tonly.form\n"), want="fired nowhere"),
    _tree(_APPLIES, side=(SIDE, _SIDE_HEAD + "fast.path\tD1\t\t-\tq1\t-\nonly.form\tD2\tevery scan\t-\tq2\t-\n"),
          want="trigger and a motivator"),
    _tree(_APPLIES, side=(SIDE, _SIDE_HEAD + "fast.path\tD1\ta filter\t-\tq1\t-\n"), want="only.form"),
    _tree(_APPLIES, side=(SIDE, _SIDE_HEAD + "fast.path\tD1\ta filter\t-\t-\t-\nonly.form\tD2\tevery scan\t-\tq2\t-\n"),
          want="road fast.path"),
    _tree("fn lower() {}\n", debt=["fast.path\treader"], base=[], want="added to the debt"),
    _tree("fn lower() {}\n", debt=["fast.path\treader,side"], base=["fast.path\treader"], want="gained side"),
    _tree("fn lower(px: &Px) -> bool { let r = FAST_PATH; let _ = r; px.ready() }\n", want="no reader"),
    _tree("fn lower(px: &Px) -> bool { strategy(FAST_PATH).is_some() }\n", debt=["fast.path\treaders"], want="a row is"),
)
CLEAN_TOML = _tree(_APPLIES)
MORE_CLEAN_TOML = (
    _tree("fn lower(px: &Px) -> bool { px.strategies.has(strategy::FAST_PATH) }\n"),
    _tree("fn lower() -> bool { !crate::face::strategy::off().has(FAST_PATH) }\n"),
    _tree("crate::hook_variants! {\n    \"H01\" pub enum Mask {\n        Fast = Strategy(FAST_PATH),\n    }\n}\n"
          "fn lower() -> bool { crate::face::mask::Mask::Fast.on() }\n"),
    _tree("fn on(px: &Px, r: Strategy) -> bool { px.strategies.has(r) }\nfn lower(px: &Px) -> bool { on(px, FAST_PATH) }\n"),
    _tree("fn lower(env: &Env) -> bool {\n    for (form, r) in [(Form::A, FAST_PATH), (Form::B, ONLY_FORM)] {\n"
          "        if env.applies(r) { return true; }\n    }\n    false\n}\n"),
    _tree("fn lower(px: &Px, f: u8) -> bool {\n    let road = match f { 0 => FAST_PATH, _ => ONLY_FORM };\n"
          "    px.strategies.has(road)\n}\n"),
    _tree("pub fn ext_rows() -> &'static [(Option<Strategy>, Mech)] { &[(None, A), (Some(strategy::FAST_PATH), B)] }\n"),
    _tree("fn lower() {}\n", (G + "decompose/src/self_spool.rs",
                              "pub const ROAD_FAST: &str = \"fast.path\";\nfn keep(b: &B) -> bool { b.facts.default_on_road_on(ROAD_FAST) }\n")),
    _tree("fn lower() {}\n", debt=["fast.path\treader"], base=["fast.path\treader,fire"]),
    _tree(_APPLIES, debt=["fast.path\treader"], base=["fast.path\treader"]),
)
