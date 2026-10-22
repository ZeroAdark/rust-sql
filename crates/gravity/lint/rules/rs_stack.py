"""RS: a recursion with no stack check. In every gravity crate (the plan reader, the lowerers, expr, exec's admission
and runner walks, fn's type semantics, the hosts, the driver, the IR and the code generators) a recursion can follow the
depth of the user's plan, expression, type or program, so a deep enough statement runs it into the thread's stack end
(SIGSEGV: the server goes down) unless some level of it calls PostgreSQL's check_stack_depth, which raises 54001 'stack
depth limit exceeded' first (D3748, D3832, D3833). The rule: every recursion cycle of a crate's call graph passes through
a function whose first statement is a stack check: decompose `crate::walk::stack_check()`, the lowerer
`crate::stack_check()`, a `stack_depth_core::check_stack_depth*` call (`check_stack_depth()?` where the function returns a
PgResult, else `check_stack_depth_or_panic()`), or, where an error cannot unwind, `if stack_depth_core::stack_is_too_deep()
{ .. }` returning that code's own error. Out of scope: the lint crate and the test-only packages (`*_it`, `*_rig`).

The call graph is read from the source text, one crate at a time (a cycle through another crate's callback is not seen): a bare call `f(`
(the same file's free fn `f`, else the crate's only free fn `f`), `self.f(` / `Self::f(` (a method of the same type, or
a trait's default method), `Type::f(` (a method of `Type`), `module::f(` (a free fn of this crate's files whose module is
`module`, `crate::` / `self::` / `super::` by position; a module the crate does not have is another crate's: no edge). A
bare call takes a fn nested in the caller or in a fn around it first. A method call on another receiver (`x.f(`) gives
no edge: its type is not known here, and the name alone joins unrelated types. A fn passed as a call's argument
(`.any(f)`, `x.is_some_and(f)`, `walk(n, f)`) counts as a call of it (`_value_refs`). A call inside a nested fn's body
is the nested fn's; a call inside a closure is its fn's.

The registry, lint/STACK-DEBT (a directory, one row per file), lists the functions that are to take the check and do not
yet: path, function (`Type::name` for a method, `name` for a free fn), crate, and the cycle the check breaks. A listed
function counts as a check. A cycle left with neither fails, at the function whose check breaks the most of it. On a
landing (--base) a row the parent lacks fails (the list only shrinks), unless it replaces a row of the same function the
landing removes (a move). A row whose function its file no longer defines, already starts with the check, or lies on no
unchecked cycle is a note (delete the row)."""
import collections
import re

from common import G, crate, generated, in_src, ledger_loc, read, Src, walk_sources

ID = "RS"
TITLE = "a recursion cycle passes through no function whose first statement is a stack check"
INSTEAD = ("a stack check as the first statement of one of the cycle's functions: decompose `crate::walk::stack_check();`, "
           "the lowerer `crate::stack_check();`, else `stack_depth_core::check_stack_depth_or_panic();` (54001, never "
           "the stack end); not yet checked: a lint/STACK-DEBT row (the list only shrinks)")
GLOBAL = True
DEBT = G + "lint/STACK-DEBT"
CRATES = None                          # every gravity crate (C6, lane guards); a tuple narrows it (the scratch tools)


def _in_scope(c):
    return c is not None and c != "lint" and not c.endswith(("_it", "_rig")) and (CRATES is None or c in CRATES)


def SCOPE(p):
    return in_src(p) and _in_scope(crate(p))


# A recursion whose depth is a small constant by construction, not the user's input: the function counts as checked. Each
# row names why; a row whose function is gone or lies on no unchecked cycle fails (delete it). Not a debt list: a row here
# is a proof the depth cannot grow, reviewed like the rule itself (a stack check would cost a per-row path for nothing).
BOUNDED = {
    (G + "kernels/src/table/fold_entries.rs", "locate"):
        "one level: the retry after the estimate's bypass turns on takes the bypass branch at the top and returns",
    (G + "kernels/src/dict/strategies/heads.rs", "walk"):
        "one level: the retry passes memo None, and only a Some(memo) pass recurses",
    (G + "exec/src/ops/subplan/raw.rs", "set_op"):
        "one level: a set's redirect names its twin (raw.rs twin_run), whose own redirect is never set",
    (G + "dict/src/regexp/program.rs", "Run::run"):
        "depth <= REGEXP_MAX_OPS (32): each level runs the next op of a program the compiler capped",
    (G + "dict/src/regexp/program.rs", "Compiler::seq"):
        "one group level: seq(true) refuses a nested group (in_group / ngroups)",
    (G + "backend/interp/src/lib.rs", "Interp::run_depth"):
        "depth <= INTERP_MAX_DEPTH (512): a deeper call traps (Trap::StackOverflow)",
    (G + "fn/src/typesem/mod.rs", "build"):
        "depth <= 16: a type nested deeper is not built (depth > 16)",
    (G + "fn/src/typesem/mod.rs", "bucket_build"):
        "depth <= 16: a type nested deeper has no bucket form (depth > 16)",
    (G + "fn/src/typesem/mod.rs", "walk"):
        "depth <= the Form's, which build caps at 16 levels; the decompressed retry is one level (its bytes are flat)",
    (G + "fn/src/typesem/mod.rs", "jsonb"):
        "depth <= 256: a deeper container is an error (depth > 256)",
    (G + "fn/src/typesem/keysem/user.rs", "cmp_agrees"):
        "depth <= 16: a type nested deeper does not agree (depth > 16)",
    (G + "fn/src/typesem/keysem/user.rs", "hash_agrees"):
        "depth <= 16: a type nested deeper does not agree (depth > 16)",
    (G + "fn/src/typesem/pgrc.rs", "walk"):
        "depth <= max_depth (callers pass JSONB_PATH_MAX, 4): prefix.len() >= max_depth stops the walk",
    (G + "ir/src/promote.rs", "Promoter::read_variable"):
        "depth <= max_depth (the lowerer passes limits::PROMOTE_MAX_READ_DEPTH, 2048): a deeper predecessor walk is "
        "Err(TooDeep) (the lowerer keeps the function unpromoted); read_variable_recursive and add_phi_operands recurse "
        "only through it",
    (G + "ir/src/text.rs", "read_one"):
        "depth <= MAX_SEXP_DEPTH (64, the IR text grammar nests six): a list nested deeper is an error",
    (G + "parallel/src/merge.rs", "fold_sub"):
        "depth < the route's bits / 3: a deeper split gives the partition back (Depth::new is None); the reread retry "
        "runs once per depth (it passes groups = est)",
}


_FN = re.compile(r"\bfn\s+([A-Za-z_]\w*)")
_BLOCK = re.compile(r"\b(impl|trait)\b")
_ITEM_START = re.compile(r"(?:^|[{};\]])\s*(?:pub(?:\s*\([^()]*\))?\s+)?(?:(?:unsafe|auto|default)\s+)*$")
CHECK = re.compile(r"\{\s*(?:(?:\w+\s*::\s*)*(?:stack_check\s*\(\s*\)\s*;|check_stack_depth\w*\s*\()"
                   r"|if\s+(?:\w+\s*::\s*)*stack_is_too_deep\s*\(\s*\)\s*\{)")
_CALL = re.compile(r"(self\s*\.\s*|Self\s*::\s*|\.\s*|([A-Za-z_]\w*)\s*::\s*)?\b([A-Za-z_]\w*)\s*(?:::\s*<[^()]*?>\s*)?\(")
_SIG = re.compile(r"\bfn\s*$")
_PARAM = re.compile(r"(?:^|[,(])\s*(?:mut\s+)?([a-z_]\w*)\s*:(?!:)")
_LET = re.compile(r"\blet\s+(?:mut\s+)?([a-z_]\w*)\b")
_PRELUDE = {"drop"}                  # std's prelude fns: a bare call in another file is std's, not the crate's namesake
_NOT_CALL = {"if", "while", "for", "match", "return", "loop", "fn", "in", "as", "move", "Some", "Ok", "Err", "None", "Box"}


_BRACKETS = {"{": re.compile(r"[{}]"), "(": re.compile(r"[()]")}


def _close(t, i, o, c):
    """-> the index after the bracket that closes the one at t[i] (`o`), or -1."""
    depth = 0
    rx = _BRACKETS.get(o)
    if rx:
        for m in rx.finditer(t, i):
            if t[m.start()] == o:
                depth += 1
            else:
                depth -= 1
                if depth == 0:
                    return m.start() + 1
        return -1
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


def _trait_impl(head):
    """`impl Trait for Type`: its methods lose to an inherent method of the same name (Rust's resolution)."""
    h = re.sub(r"<[^<>]*(?:<[^<>]*(?:<[^<>]*>[^<>]*)*>[^<>]*)*>", "", head).split(" where ")[0]
    return " for " in h


def _block_type(kind, head):
    """The type an `impl` block's functions are methods of (`trait X` for a trait's default methods)."""
    if kind == "trait":
        m = re.match(r"\s*([A-Za-z_]\w*)", head)
        return "trait " + (m.group(1) if m else "?")
    h = re.sub(r"<[^<>]*(?:<[^<>]*(?:<[^<>]*>[^<>]*)*>[^<>]*)*>", "", head).split(" where ")[0]
    seg = h.split(" for ")[-1].strip()
    m = re.search(r"([A-Za-z_]\w*)\s*$", seg.split("::")[-1].strip()) if seg else None
    return m.group(1) if m else "?"


def items(t):
    """-> (impl / trait blocks [(body start, body end, type)], fns [(name, line, body start, body end, `fn` at, parameter
    list span)]) of one file's code text."""
    blocks = []
    for m in _BLOCK.finditer(t):
        if not _ITEM_START.search(t, max(0, m.start() - 80), m.start()):
            continue                          # `impl Trait` in a signature, not an item
        j, s = t.find("{", m.end()), t.find(";", m.end())
        if j < 0 or 0 <= s < j:
            continue
        k = _close(t, j, "{", "}")
        if k > 0:
            head = t[m.end():j]
            blocks.append((j, k, _block_type(m.group(1), head), m.group(1) == "impl" and _trait_impl(head)))
    fns = []
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
        rest = t[j:]
        b, s = rest.find("{"), rest.find(";")
        if b < 0 or 0 <= s < b:
            continue
        k = _close(t, j + b, "{", "}")
        if k > 0:
            fns.append((m.group(1), t.count("\n", 0, m.start()) + 1, j + b, k, m.start(), (i, j)))
    return blocks, fns


def _modpath(p):
    rel = p.split("/src/", 1)[1][:-3].split("/")
    return rel[:-1] if rel[-1] in ("mod", "lib", "main") else rel


class Fn:
    __slots__ = ("path", "crate", "mod", "name", "line", "ty", "check", "a", "b", "nested", "outer", "locals", "t", "timpl", "at")

    def label(self):
        """The ledger's spelling: `Type::name` for a method, `name` for a free fn."""
        return (self.ty[6:] if self.ty.startswith("trait ") else self.ty) + "::" + self.name if self.ty else self.name


def graph(ctx):
    """-> ({id: Fn}, {id: set of called ids}) over the scope crates; an id is (path, name, line)."""
    nodes, by_name = {}, collections.defaultdict(list)
    for p in walk_sources(ctx.root):
        if not SCOPE(p):
            continue
        src = getattr(ctx, "srcs", {}).get(p)
        if src is None:
            t0 = ctx.read(p) if hasattr(ctx, "read") else read(ctx.root, p)
            if t0 is None or generated(t0):
                continue
            src = Src(p, t0)
        t = src.joined("code")
        blocks, fns = items(t)
        c, mod, open_fns = crate(p), _modpath(p), []
        for name, line, a, b, at, (pa, pb) in fns:   # in text order: the open bodies around `at` form a stack
            while open_fns and open_fns[-1].b <= at:
                open_fns.pop()
            outer = open_fns[-1] if open_fns else None
            around = [bl for bl in blocks if bl[0] < at < bl[1]]
            inner = min(around, key=lambda bl: bl[1] - bl[0]) if around else None
            if inner and outer and inner[0] < outer.a:
                inner = None                  # a fn nested in another fn's body is no method of the block around that fn
            f = Fn()
            f.path, f.crate, f.mod, f.name, f.line, f.a, f.b, f.t = p, c, mod, name, line, a, b, t
            f.ty, f.check, f.nested = inner[2] if inner else None, bool(CHECK.match(t, a)), []
            f.timpl, f.at = bool(inner and inner[3]), at
            f.outer = (p, outer.name, outer.line) if outer else None
            f.locals = set(_PARAM.findall(t, pa, pb)) | set(_LET.findall(t, a, b))
            if outer:
                outer.nested.append((a, b))
            open_fns.append(f)
            nodes[(p, name, line)] = f
            by_name[(c, name)].append((p, name, line))
    roots = collections.defaultdict(set)       # each crate's top-level module names (a path's first segment)
    for f in nodes.values():
        if f.mod:
            roots[f.crate].add(f.mod[0])
    edges = {}
    for nid, n in nodes.items():
        out, t = set(), n.t
        for m in _CALL.finditer(t, n.a, n.b):
            name = m.group(3)
            cands = by_name.get((n.crate, name))
            if not cands or name in _NOT_CALL or any(x <= m.start() < y for x, y in n.nested):
                continue
            if _SIG.search(t, max(0, m.start() - 8), m.start()):
                continue                      # a nested fn's signature
            pre, qual = (m.group(1) or "").replace(" ", ""), m.group(2)
            if pre.startswith("self.") or pre.startswith("Self::"):
                tg = _inherent_first([x for x in cands if nodes[x].ty == n.ty or (nodes[x].ty or "").startswith("trait ")], nodes)
            elif qual and qual[0].isupper():
                tg = _inherent_first([x for x in cands if nodes[x].ty in (qual, "trait " + qual)], nodes)
            elif qual:
                free = [x for x in cands if nodes[x].ty is None and nodes[x].outer is None]
                if qual == "self":
                    tg = [x for x in free if nodes[x].path == n.path]
                elif qual == "super":
                    tg = [x for x in free if nodes[x].mod == n.mod[:-1]]
                elif qual == "crate":
                    tg = [x for x in free if nodes[x].mod == []] or (free if len(free) == 1 else [])
                elif _foreign_path(t, m.start(2), roots[n.crate]):
                    continue                  # `other_crate::m::f(`: a path rooted outside this crate
                else:                         # a module of this crate (else another crate's: no edge)
                    tg = [x for x in free if nodes[x].mod and nodes[x].mod[-1] == qual]
            elif pre == ".":
                continue                      # a method on another receiver: no edge (module doc)
            elif name in n.locals:
                continue                      # a parameter or a `let` of the caller (a fn pointer, a closure) shadows it
            else:                             # the innermost block's item first: a fn nested in this fn or around it
                scopes, up = [], nid
                while up is not None:
                    scopes.append(up)
                    up = nodes[up].outer
                free = [x for x in cands if nodes[x].ty is None and (nodes[x].outer is None or nodes[x].outer in scopes)]
                local = [[x for x in free if nodes[x].outer == s] for s in scopes]
                tg = next((xs for xs in local if xs), None) or [x for x in free if nodes[x].path == n.path] \
                    or (free if len(free) == 1 and name not in _PRELUDE else [])
            out.update(tg)
        out.update(_value_refs(t, n, nid, nodes, by_name, roots[n.crate]))
        edges[nid] = out
    return nodes, edges


_VALUE = re.compile(r"[(,]\s*([A-Za-z_]\w*)\s*(?=[,)])")
_ARG_HEAD = re.compile(r"(\.\s*)?((?:[A-Za-z_]\w*\s*::\s*)*)([A-Za-z_]\w*)\s*(?:::\s*<[^()]*?>\s*)?$")


def _open_paren(t, i, lo):
    """The `(` of the innermost call around `t[i]` (None when a `[` or `{` closes over it first)."""
    d = 0
    while i > lo:
        i -= 1
        if t[i] in ")]}":
            d += 1
        elif t[i] in "([{":
            if d == 0:
                return i if t[i] == "(" else None
            d -= 1
    return None


def _value_refs(t, n, nid, nodes, by_name, roots):
    """The fns `n` passes as a value to a call (`.any(f)`, `p.lefttree.is_some_and(f)`, `.map_or(0, f)`, `dup_inner(x,
    f)`): the callee calls it, so it counts as a call (a plan walk that recurses through a combinator). Only where every
    bare mention of the name in `n`'s body is such an argument: a local of that name (a parameter, a `let` or match
    pattern, a closure's) is mentioned elsewhere too, and a constructor (`Some(f)`, a struct's field), a tuple or another
    crate's fn (a hook's registration) does not call it here."""
    out, body, seen = set(), t[n.a:n.b], set()
    for m in _VALUE.finditer(t, n.a, n.b):
        name = m.group(1)
        if name in seen or name in _NOT_CALL or (n.crate, name) not in by_name or name in n.locals:
            continue
        seen.add(name)
        if any(any(x <= n.a + o.start() < y for x, y in n.nested) or not _passed(t, n, n.a + o.start(), roots)
               for o in re.finditer(r"(?<![\w.:])%s\b(?!\s*(?:\(|::|!))" % re.escape(name), body)):
            continue
        scopes, up = [], nid
        while up is not None:
            scopes.append(up)
            up = nodes[up].outer
        free = [x for x in by_name[(n.crate, name)] if nodes[x].ty is None and (nodes[x].outer is None or nodes[x].outer in scopes)]
        local = [[x for x in free if nodes[x].outer == s] for s in scopes]
        out.update(next((xs for xs in local if xs), None) or [x for x in free if nodes[x].path == n.path]
                   or (free if len(free) == 1 else []))
    return out


def _passed(t, n, i, roots):
    """The mention at `t[i]` is an argument of a method call or of a call of this crate's fn."""
    if not re.search(r"[(,]\s*$", t[max(n.a, i - 40):i]) or not re.match(r"\w+\s*[,)]", t[i:i + 200]):
        return False
    at = _open_paren(t, i, n.a)
    h = _ARG_HEAD.search(t, max(n.a, at - 200), at) if at is not None else None
    if h is None:
        return False                          # a tuple
    if h.group(1):
        return True                           # a method
    last = h.group(3)
    return not (last[0].isupper() or last in _NOT_CALL or last == "let" or h.group(2) and _foreign_path(t, h.start(3), roots))


_PATH_HEAD = re.compile(r"((?:[A-Za-z_]\w*\s*::\s*)+)$")


def _foreign_path(t, at, roots):
    """The call's path `a::b::m::f(` (its segments before `m`, which starts at `at`) is rooted in another crate: a first
    segment that is neither crate / self / super nor one of this crate's top-level modules."""
    m = _PATH_HEAD.search(t, max(0, at - 200), at)
    if not m:
        return False
    first = m.group(1).split("::")[0].strip()
    return first not in ("crate", "self", "super", "Self") and first not in roots


def _inherent_first(tg, nodes):
    """A method of the type's own impl wins over a trait impl's of the same name (`impl T for X { fn f(&self) {
    X::f(self) } }` calls X's inherent f, not itself)."""
    inh = [x for x in tg if not nodes[x].timpl and not (nodes[x].ty or "").startswith("trait ")]
    return inh or tg


def cycles(vs, edges):
    """-> the strongly connected components of the graph on `vs` that hold a cycle (iterative Tarjan), each sorted."""
    index, low, on, stack, out, n = {}, {}, set(), [], [], 0
    for root in sorted(vs):
        if root in index:
            continue
        work = [(root, iter(sorted(w for w in edges.get(root, ()) if w in vs)))]
        index[root] = low[root] = n
        n += 1
        stack.append(root)
        on.add(root)
        while work:
            v, it = work[-1]
            w = next(it, None)
            if w is not None:
                if w not in index:
                    index[w] = low[w] = n
                    n += 1
                    stack.append(w)
                    on.add(w)
                    work.append((w, iter(sorted(x for x in edges.get(w, ()) if x in vs))))
                elif w in on:
                    low[v] = min(low[v], index[w])
                continue
            work.pop()
            if work:
                low[work[-1][0]] = min(low[work[-1][0]], low[v])
            if low[v] == index[v]:
                comp = []
                while True:
                    x = stack.pop()
                    on.discard(x)
                    comp.append(x)
                    if x == v:
                        break
                if len(comp) > 1 or v in edges.get(v, ()):
                    out.append(sorted(comp))
    return out


def breakers(vs, edges):
    """-> [(id, the cycle it breaks)]: a greedy set of functions whose checks break every cycle on `vs` (in each cyclic
    component, the function with the most in-component calls in and out; a self-call is its own)."""
    vs, out = set(vs), []
    while True:
        comps = cycles(vs, edges)
        if not comps:
            return out
        for comp in comps:
            cs = set(comp)
            if len(comp) == 1:
                best = comp[0]
            else:
                best = max(comp, key=lambda v: (len(edges[v] & cs) * sum(1 for u in comp if v in edges[u]),
                                                -v[2], v[0], v[1]))
            out.append((best, comp))
            vs.discard(best)


def debt_file(path, label):
    """A STACK-DEBT row's file name: <path below crates/gravity, '/' as '.'>@<function, '::' as '.'>."""
    return path[len(G):].replace("/", ".") + "@" + label.replace("::", ".")


def cycle_note(nodes, best, comp):
    others = [nodes[v].label() for v in comp if v != best]
    if not others:
        return "calls itself"
    names = sorted(set(others))
    return "with " + ", ".join(names[:4]) + (f" (+{len(names) - 4})" if len(names) > 4 else "")


def _base_scope(ctx):
    """The crates the parent's RS covered (its `CRATES` tuple), or None when it covered every crate (or reads no tuple)."""
    src = ctx.base_read(G + "lint/rules/rs_stack.py") if getattr(ctx, "base_read", None) else None
    m = re.search(r"^CRATES\s*=\s*\(([^)]*)\)", src or "", re.M)
    return set(re.findall(r'"([^"]+)"', m.group(1))) if m else None


def parse_debt(text):
    """-> ({(path, fn): (line, crate)}, [(line, problem)])."""
    rows, bad = {}, []
    for n, raw in enumerate(text.split("\n"), 1):
        s = raw.split("#", 1)[0].rstrip()
        if not s.strip():
            continue
        f = s.split("\t")
        if len(f) < 4 or not all(x.strip() for x in f[:4]):
            bad.append((n, "a row is path<TAB>fn<TAB>crate<TAB>the cycle its check breaks"))
            continue
        key, c = (f[0].strip(), f[1].strip()), f[2].strip()
        if c != crate(key[0]) or not SCOPE(key[0]):
            bad.append((n, f"{key[0]}: crate {c!r} is not the path's, or the path is not in {', '.join(CRATES)}"))
            continue
        if key in rows:
            bad.append((n, f"duplicate row {key[0]} {key[1]}"))
        rows[key] = (n, c)
    return rows, bad


def check_tree(ctx):
    out = []
    text, at = ctx.ledger(DEBT)
    rows, bad = parse_debt(text) if text is not None else ({}, [])
    out += [(*at(n), note) for n, note in bad]
    nodes, edges = graph(ctx)
    debt = {v for v, f in nodes.items() if (f.path, f.label()) in rows or (f.path, f.label()) in BOUNDED}
    open_ = {v for v, f in nodes.items() if not f.check}
    for v, comp in breakers(open_ - debt, edges):
        f = nodes[v]
        out.append((f.path, f.line, f"`{f.label()}` is on a recursion cycle ({cycle_note(nodes, v, comp)}) that passes no "
                                    f"stack check; instead: {INSTEAD}"))
    on_cycle = {v for comp in cycles(open_, edges) for v in comp}
    labels = collections.defaultdict(dict)
    for v, f in nodes.items():
        labels[f.path].setdefault(f.label(), []).append(v)
    for (p, fn), (n, _) in sorted(rows.items(), key=lambda kv: kv[1][0]):
        vs = labels.get(p, {}).get(fn)
        why = ("is not in its file" if not vs else "starts with the stack check" if all(nodes[v].check for v in vs)
               else "is on no unchecked recursion cycle" if not any(v in on_cycle for v in vs) else None)
        if why:
            ctx.notes.append(f"RS: {ledger_loc(at, n)}: {p} `{fn}` {why}: delete the row")
    present = {f.crate for f in nodes.values()}
    for (p, fn), why_bounded in sorted(BOUNDED.items()):
        if crate(p) not in present:
            continue                          # a tree without that crate (the selftest's)
        vs = labels.get(p, {}).get(fn)
        why = ("is not in its file" if not vs else "starts with the stack check" if all(nodes[v].check for v in vs)
               else "is on no unchecked recursion cycle" if not any(v in on_cycle for v in vs) else None)
        if why:
            out.append((p, 0, f"BOUNDED row `{fn}` {why}: delete the row (rs_stack.py BOUNDED)"))
    if rows:
        per = collections.Counter(c for _, c in rows.values())
        ctx.notes.append(f"RS: STACK-DEBT {len(rows)} rows (" + ", ".join(f"{c} {k}" for c, k in sorted(per.items())) + ")")
    base_text = ctx.base_ledger(DEBT) if ctx.base_ledger else None
    if base_text is not None:
        base, _ = parse_debt(base_text)
        removed = {bf for bp, bf in base if (bp, bf) not in rows}
        covered = _base_scope(ctx)
        for (p, fn), (n, _) in sorted(rows.items(), key=lambda kv: kv[1][0]):
            if covered is not None and crate(p) not in covered:
                continue                      # a crate the parent's RS did not cover: the landing that brings it in
            if (p, fn) not in base and fn not in removed:
                out.append((*at(n), f"{p} `{fn}` added: the list only shrinks (start the function with a stack check)"))
    return out


PLANTED_TOML = (G + "expr/src/planted.rs",
                "fn depth(e: &Expr) -> usize { match e { Expr::Op(a, b) => 1 + depth(a).max(depth(b)), _ => 1 } }\n")
CLEAN_TOML = (G + "expr/src/clean.rs",
              "fn depth(e: &Expr) -> usize {\n    // one level of the user's expression\n    stack_check();\n"
              "    match e { Expr::Op(a, b) => 1 + depth(a).max(depth(b)), _ => 1 }\n}\n"
              "fn leaf(e: &Expr) -> usize { e.depth() + other::depth(e) }\n")
MORE_PLANTED_TOML = (
    (G + "lowerv4/src/planted.rs",            # a cycle through methods, checked off it
     "impl Lower<'_> {\n    fn step(&mut self, n: Node) { if !self.nested(n) { Self::chain(self, n) } }\n"
     "    fn chain(&mut self, n: Node) { crate::stack_check(); self.step(n) }\n"
     "    fn nested(&mut self, n: Node) -> bool { self.step(n.inner()); true }\n}\n"),
    (G + "fn/src/planted.rs",                 # a crate beyond the first four
     "fn depth(e: &Expr) -> usize { 1 + depth(e) }\n"),
    (G + "decompose/src/planted.rs",          # a trait method that calls itself through Self::, the check not first
     "trait Walk {\n    fn walk(&self, n: Node) { let k = n.kids(); crate::walk::stack_check(); Self::walk(self, k) }\n}\n"),
    (G + "driver/src/planted.rs",             # a plan walk that recurses through a combinator, passing itself
     "fn joins(n: Node) -> bool { n.is_join() || n.lefttree.is_some_and(joins) }\n"),
)
MORE_CLEAN_TOML = (
    (G + "lowerv4/src/clean.rs",              # every cycle passes a checked fn; `impl Fn` in a signature is no block
     "impl Lower<'_> {\n    fn step(&mut self, n: Node, f: impl Fn(Node) -> bool) {\n        crate::stack_check();\n"
     "        if !self.nested(n) { self.step(n, f) }\n    }\n"
     "    fn nested(&mut self, n: Node) -> bool { self.step(n.inner(), |_| true); true }\n}\n"),
    (G + "decompose/src/clean.rs",            # a nested fn of the same name, a receiver call, another type's method
     "fn walk(n: Node) -> u32 {\n    fn walk(k: u32) -> u32 { k + 1 }\n    walk(n.id()) + n.walk() + Other::walk(n)\n}\n"),
    (G + "lowerv4_it/src/clean.rs",           # a test-only package: out of scope
     "fn depth(e: &Expr) -> usize { 1 + depth(e) }\n"),
    (G + "lower/src/clean.rs",                # a parameter or a `let` named as a fn shadows it
     "fn decide(rows: u64, decide: fn(u64) -> u64) -> u64 { decide_on(rows, decide) }\n"
     "fn decide_on(v: u64, decide: fn(u64) -> u64) -> u64 { decide(v) }\n"
     "fn walk(n: u32) -> u32 { let walk = |k: u32| k + 1; walk(n) }\n"),
    (G + "hosts/src/clean.rs",                # the non-panicking form, where an error cannot unwind
     "fn depth(e: &Expr) -> Result<usize, Fault> {\n    if stack_depth_core::stack_is_too_deep() {\n"
     "        return Err(Fault::DEPTH);\n    }\n    Ok(1 + depth(e.kid())?)\n}\n"),
    (G + "lowerv4/src/clean2.rs",             # a trait impl's method calls the inherent one of the same name
     "impl Hook for Dense {\n    fn lo(&self) -> i64 { Dense::lo(self) }\n    fn hi(&self) -> i64 { self.hi() }\n}\n"
     "impl Dense {\n    fn lo(&self) -> i64 { 1 }\n    fn hi(&self) -> i64 { 2 }\n}\n"),
    (G + "lowerv4/src/served.rs",             # another crate's path whose last module shares this file's name
     "pub fn served(x: u32) -> u32 { types_core::served::served(x) }\n"),
    (G + "driver/src/clean.rs",               # its own name as a value: a local, a constructor's field, a hook
     "fn less(b: &B) -> V { let (less, eq) = (b.lt(), b.eq()); b.and(less, eq) }\n"
     "fn entry(a: u64) -> W { W { main: Some(entry), arg: a } }\n"
     "fn spill(n: u32) -> bool { other_crate::set_hook(spill); n > 0 }\n"),
)
