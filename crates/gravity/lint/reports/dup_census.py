#!/usr/bin/env python3
"""Duplication census (report-only): every implementation of each decision kind in crates/gravity, with near-copies
clustered. It fails nothing; it counts how many places decide the same question, so the restructuring can show each
decision falling to one implementation in its home.

    dup_census.py [--root DIR] [--file-map FILE-MAP.tsv] [--tsv OUT] [--md OUT] [--label TEXT]
    dup_census.py --selftest

An implementation is a function (or a module-level item, fn "-") that decides one of the kinds below itself, found by
(a) a construct in its body (`use`: a pg_proc column, an OID bound, a GUC read, a key support lookup), charged to the
innermost function holding it, or (b) its name (`name`: a function returning bool / Option whose name says it decides,
e.g. `raise_free`, `user_fn`). Calls of the home's shared primitive are not implementations. A comment saying the code
copies another walk ("copy of", "same body as", "<x>'s walk") is listed as `via copy` under the kind of its function.

Clusters group a kind's implementations whose normalized bodies are near-identical (token sequence similarity >= 0.80;
local names normalized, calls / paths / fields kept; a long function contributes the lines around its hits). Every
implementation has a cluster id, so clusters = distinct implementations after folding near-copies.

Owner lane: FILE-MAP.tsv (today / target paths, lowerv4 == lower), the file's own row first, else the ops/<family>/
directory's lane, else the lane holding most rows under the longest shared directory prefix.

Output: one TSV row per hit (kind, path, line, fn, fn_line, owner, home, cluster, via, text) and a markdown summary.
Python 3, standard library; it reads the tree as text (lint/rules/common.py's lexer) and never imports from it."""
import argparse
import collections
import difflib
import os
import re
import shutil
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "rules"))
from common import G, HOME, L, Src, families, generated, in_any, read, walk_sources  # noqa: E402
from rl_lifetime import functions  # noqa: E402
import rk_keysem  # noqa: E402

# The effects primitive proper; a family's moved decisions parked in effects/visible/<family>.rs (and fn/src/typesem/
# <family>.rs for keys) are not the primitive yet: their UNIFY switches them to it and deletes them.
EFFECTS = [G + "decompose/src/effects/"] + L("effects.rs") + L("effects/visible.rs")
PARKED_EFFECTS = L("effects/visible/")
PGPROC_ROWS = [G + "expr/src/catalog.rs", G + "hosts/src/facts.rs", G + "decompose/src/catalog/"]
KEYSEM = [G + "fn/src/typesem/", G + "fn/src/cmpfn.rs", G + "fn/src/resolved.rs"]
CATALOG = [G + "decompose/src/catalog/", G + "hosts/src/facts.rs"]
INPUTS = HOME["inputs"] + [G + "hosts/src/inputs.rs", G + "lower/src/inputs/"]


def _rx(s):
    return re.compile(s)


# kind -> (title, home globs, shared primitive, name pattern (on the fn name) or None, use pattern (on code) or None)
KINDS = collections.OrderedDict([
    ("raise-free", ("may this expression raise: raise-free / quiet / pure walks", EFFECTS,
                    "effects::visible::error_visible; decompose::effects::{pure, quiet_call, quiet_args}",
                    _rx(r"raise_free|can_raise|may_raise|cannot_raise|never_raise|(?:^|_)(?:raises|raising|quiet|loud|pure"
                        r"|nonraising|error_free|can_error|may_error)(?:_|$)"), None)),
    ("volatile", ("volatility: provolatile / VOLATILE / nextval / random tests", EFFECTS + PGPROC_ROWS,
                  "effects::visible::calls_visible / expr::volatile; decompose::effects::of_call(..).acts",
                  _rx(r"(?:^|_)(?:volatile|volatility|nonvolatile|acts|immutable)(?:_|$)"),
                  _rx(r"\bprovolatile\b|\bVolatility::\w+|\bvolatility\b[^;\n]{0,40}b'[vsi]'|b'[vsi]'[^;\n]{0,40}\bvolatility\b"
                      r"|\bcontain_volatile_functions\b|\bfunc_volatile\s*\(|\bPROVOLATILE_\w+"
                      r"|[=!]=\s*F_(?:NEXTVAL|SETVAL|CURRVAL|LASTVAL|RANDOM|SETSEED|CLOCK_TIMESTAMP|TIMEOFDAY|GEN_RANDOM_UUID)\w*"
                      r"|\bF_(?:NEXTVAL|SETVAL|CURRVAL|LASTVAL|RANDOM|SETSEED|CLOCK_TIMESTAMP|TIMEOFDAY|GEN_RANDOM_UUID)\w*\s*(?:[=!]=|\|)"))),
    ("leakproof", ("leakproofness: proleakproof tests", EFFECTS + PGPROC_ROWS, "decompose::effects::leakproof",
                   _rx(r"leakproof|(?:^|_)leaky(?:_|$)"),
                   _rx(r"\b(?:pro)?leakproof\b|\bget_func_leakproof\s*\("))),
    # a catalog row's strict flag read to decide something; an evaluator's `call.strict` NULL short-circuit is PG's own
    # function-call semantics, not a copy of the decision
    ("strict", ("strictness: proisstrict tests over a catalog row", EFFECTS + PGPROC_ROWS, "decompose::effects (FuncProps strict)",
                None,
                _rx(r"\bproisstrict\b|\bfunc_strict\s*\(|\b(?:func|row|fmgr_isbuiltin|builtin_func|proc)\s*\([^;{}]{0,80}?\.strict\b"))),
    ("user-fn", ("is this a user function: FIRST_NORMAL_OID / user_fn tests", EFFECTS + [G + "decompose/src/catalog/"],
                 "decompose::effects::user_code",
                 _rx(r"user_(?:fn|fns|func|function|code|op|oper|operator|proc|defined)|(?:^|_)is_builtin(?:_|$)"
                     r"|(?:^|_)builtin_only(?:_|$)"),
                 _rx(r"\bFIRST_NORMAL_OID\b|\bFIRST_NORMAL_OBJECT_ID\b|\bFirstNormalObjectId\b|\bFIRST_UNPINNED_OBJECT_ID\b"
                     r"|[<>]=?\s*16_?384\b|\b16_?384\s*[<>]"))),
    ("key-eq", ("key equality / image equality outside KeySem", KEYSEM + CATALOG, "KeySem (gravity_fn::typesem): eq",
                _rx(r"image_eq|bitwise_eq|bytes_eq|byte_eq|eq_by_image|image_equal|same_image|key_eq|eq_form|binary_eq"
                    r"|memcmp_eq|eq_is_image|image_is_eq"), None)),
    ("key-hash", ("key hash decisions outside KeySem", KEYSEM + CATALOG, "KeySem (gravity_fn::typesem): hash",
                  _rx(r"hash_form|(?:^|_)hashable(?:_|$)|can_hash|hash_ok|hash_procs"), None)),
    ("key-order", ("key order / comparator decisions outside KeySem", KEYSEM + CATALOG, "KeySem (gravity_fn::typesem): order",
                   _rx(r"order_form|(?:^|_)sortable(?:_|$)|can_sort|cmp_form|order_image|byte_order|memcmp_order|word_order"
                       r"|(?:^|_)orderable(?:_|$)|order_family|cmp_family"), None)),
    ("key-site", ("key support call site (native / hosted / leader-only) outside KeySem", KEYSEM + CATALOG,
                  "KeySem (gravity_fn::typesem): site", None, None)),
    ("guc-read", ("run-time GUC reads outside inputs", INPUTS, "inputs homes (lower inputs/, exec inputs/, hosts inputs.rs)",
                  None,
                  _rx(r"\bguc_tables::\w+|\bguc_seams::\w+|\bguc::store::\w+|\bvars::\w+\s*\.\s*read\s*\(|\bglobals::work_mem\b"
                      r"|\bGetConfigOption\w*\s*\(|\bget_config_option\w*\s*\("))),
    ("env-read", ("environment reads outside inputs", INPUTS, "inputs homes (knobs/, driver::env)", None,
                  _rx(r"\benv::var(?:_os)?\s*\(|\benv::vars(?:_os)?\s*\("))),
    ("opfamily", ("catalog lookups of operator families / support procs", KEYSEM + CATALOG,
                  "KeySem / typesem (through the operator class); decompose catalog at plan time", None,
                  _rx(r"\b(?:get_op_opfamily_\w+|get_opfamily_\w+|get_ordering_op_\w+|get_op_hash_functions"
                      r"|get_opclass_\w+|get_equality_op_for_ordering_op|get_compatible_hash_operators"
                      r"|get_mergejoin_opfamilies|op_hashjoinable|op_mergejoinable|lookup_type_cache)\s*\("
                      r"|\bTYPECACHE_\w+|\bpg_amop\b|\bpg_amproc\b|\.(?:op_family|opfamily_proc|op_hash_functions)\s*\("))),
])
# RK's constructs (rules/rk_keysem.py USES) feed the key kinds; a match the opfamily pattern also makes is a lookup.
RK_KIND = {"eq": "key-eq", "hash": "key-hash", "order": "key-order", "site": "key-site"}
# A name-detected function whose one-expression body only asks a home (effects, visible, typesem / keysem, inputs) decides nothing
# itself: a delegate, listed but not counted.
DELEGATE_CALL = re.compile(r"\b(?:effects|visible|typesem|keysem|inputs|cmpfn|resolved)::\w+\s*\(|\bsuper::\w+\s*\(")
DELEGATE_LINES = 1
# Name detection skips: the IR optimizer's `pure` is an instruction without side effects, not an expression that cannot raise.
NAME_SKIP = {"raise-free": [G + "ir/src/"]}
STRING_VOLATILE = re.compile(r'[=!]=\s*"(?:nextval|setval|currval|lastval|random|setseed|clock_timestamp|timeofday|gen_random_uuid)"'
                             r'|"(?:nextval|setval|currval|lastval|random|setseed|clock_timestamp|timeofday|gen_random_uuid)"\s*(?:[=!]=|=>|\|)')
_USE_LINE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?use\s")
# A comment that names another file's code it copies: "copy of the same body", "same body as", "copied from x.rs",
# "x.rs `f`'s walk", "x.rs's copy", "x.rs D1005's test". "copy of `bytes`" and "`f`'s walk" alone are data copies and a
# helper's own walk here, so the copied code must sit in another file (a .rs path).
_CODE_REF = r"\b[\w/]+\.rs\b(?:\s+`?[\w:]+`?)?"
COPY = re.compile(r"\bcopy of the same (?:body|walk|test|check)\b|\bsame body as\b|\bsame (?:walk|test|check|predicate) as "
                  + _CODE_REF + r"|\bcopied (?:from|out of) " + _CODE_REF + r"|\b(?:a |the )?copy of " + _CODE_REF
                  + r"|" + _CODE_REF + r"'s (?:walk|copy|test|check)\b|\bduplicate of " + _CODE_REF
                  + r"|\b(?:kept|keep|keeps) in (?:sync|step) with " + _CODE_REF, re.I)
PG_REF = re.compile(r"\bPG\b|PostgreSQL|postgres|\.c\b|\bC's\b|\bExec[A-Z]")
_RET = re.compile(r"->\s*(?:bool\b|Option\s*<)")
_TOK = re.compile(r"[A-Za-z_][A-Za-z0-9_]*|\d+|::|->|=>|==|!=|<=|>=|&&|\|\||[^\s\w]")
_KW = set("as break const continue crate else enum extern false fn for if impl in let loop match mod move mut pub ref "
          "return self Self static struct super trait true type unsafe use where while Some None Ok Err bool u8 u16 u32 "
          "u64 usize i32 i64 matches".split())
SIM = 0.80
LONG = 80        # a function longer than this contributes only the lines around its hits
WINDOW = 4


def kind_home(kind, path, fams=()):
    """-> "yes" (the shared primitive's home), "parked" (moved into the home's directory as a family file, not unified
    yet) or "no"."""
    effects = EFFECTS[0] in KINDS[kind][1]
    if effects and in_any(path, PARKED_EFFECTS):
        return "parked"
    m = re.match(re.escape(G) + r"(?:decompose/src/effects|lowerv4/src/effects|lower/src/effects)/([a-z0-9_]+)\.rs$", path)
    if effects and m and m.group(1) in fams:      # a family's moves parked beside the effects primitive (effects/setop.rs)
        return "parked"
    if kind.startswith("key-") or kind == "opfamily":
        m = re.match(re.escape(G) + r"fn/src/typesem/([a-z0-9_]+)\.rs$", path)
        if m and m.group(1) in fams:
            return "parked"
    return "yes" if in_any(path, KINDS[kind][1]) else "no"


# ---------------------------------------------------------------------------------------------------------------------
# owner lanes from FILE-MAP.tsv

# A parked file named for its moves rather than a lane (effects/visible/merge.rs: MergeAppend's and merge join's
# decisions, moved by the append lane's EFF-MERGE-APPEND entry).
PARKED_OWNER = {"merge": "append"}


class Owners:
    def __init__(self, file_map):
        self.exact, self.rows, self.family, self.lanes = {}, [], {}, set()
        if not file_map or not os.path.isfile(file_map):
            return
        fam = collections.defaultdict(collections.Counter)
        for n, raw in enumerate(open(file_map, encoding="utf-8")):
            f = raw.rstrip("\n").split("\t")
            if n == 0 or len(f) < 4:
                continue
            lane = f[3].strip()
            if not lane or lane == "-":
                continue
            self.lanes.add(lane)
            for p in (f[1], f[0]):           # target first, then today's path
                q = self.norm(p.strip())
                if not q or "*" in q:
                    continue
                self.exact.setdefault(q, lane)
                self.rows.append((q.split("/"), lane))
                m = re.match(r"[^/]+/src/ops/([a-z0-9_]+)(?:/([a-z0-9_]+))?/", q)
                if m:
                    fam[m.group(1)][lane] += 1
                    if m.group(2):
                        fam[m.group(1) + "/" + m.group(2)][lane] += 1
        self.family = {k: c.most_common(1)[0][0] for k, c in fam.items()}

    @staticmethod
    def norm(p):
        if p.startswith(G):
            p = p[len(G):]
        return re.sub(r"^lowerv4/", "lower/", p)

    def of(self, path):
        q = self.norm(path)
        m = re.match(r"lower/src/effects/visible/([a-z0-9_]+)\.rs$", q) or re.match(r"fn/src/typesem/([a-z0-9_]+)\.rs$", q)
        if m and m.group(1) in PARKED_OWNER:     # a parked family file belongs to the lane whose moves it holds
            return PARKED_OWNER[m.group(1)]
        if m and m.group(1).replace("_", "-") in self.lanes:      # core_face.rs -> core-face
            return m.group(1).replace("_", "-")
        if q in self.exact:
            return self.exact[q]
        m = re.match(r"[^/]+/src/ops/([a-z0-9_]+)(?:/([a-z0-9_]+))?(?:/|\.rs$)", q)
        if m:
            if m.group(2) and m.group(1) + "/" + m.group(2) in self.family:
                return self.family[m.group(1) + "/" + m.group(2)]
            if m.group(1) in self.family:
                return self.family[m.group(1)]
        parts, best, votes = q.split("/"), 0, collections.Counter()
        for rp, lane in self.rows:
            k = 0
            while k < min(len(rp), len(parts)) - 1 and rp[k] == parts[k]:
                k += 1
            if k > best:
                best, votes = k, collections.Counter()
            if k == best and k > 0:
                votes[lane] += 1
        return votes.most_common(1)[0][0] if votes else "-"


# ---------------------------------------------------------------------------------------------------------------------
# the scan

def _line_of(t, off):
    return t.count("\n", 0, off) + 1


_FNS = {}


def fns_of(src):
    if src.path not in _FNS:
        _FNS[src.path] = functions(src)
    return _FNS[src.path]


def scan_file(src):
    """-> [(kind, line, fn, fn_line, via)] for one source file."""
    t = src.joined("code")
    fns = fns_of(src)
    starts = [0]
    for ln in src.code:
        starts.append(starts[-1] + len(ln) + 1)
    spans = []
    for name, line, (a, b) in fns:      # from the fn's own line (a hit in its signature is its own) to its body's end
        spans.append((starts[line - 1], b, name, line))

    def owner_fn(off):
        inner = [(b - a, name, line) for a, b, name, line in spans if a <= off < b]
        if inner:
            _, name, line = min(inner)
            return name, line
        return "-", 0

    out = []
    # (a) names
    for name, line, (a, b) in fns:
        head = t[max(0, t.rfind("fn " + name, 0, a)):a]
        rets = _RET.search(head) is not None
        body = t[a + 1:b - 1]
        lines = [x for x in body.split("\n") if x.strip()]
        delegate = len(lines) <= DELEGATE_LINES and DELEGATE_CALL.search(body) is not None
        for kind, (_, _, _, name_rx, use_rx) in KINDS.items():
            if name_rx is not None and rets and name_rx.search(name.lower()) and not in_any(src.path, NAME_SKIP.get(kind, [])):
                own = use_rx is not None and use_rx.search(body)
                out.append((kind, line, name, line, "delegate" if delegate and not own else "name"))
    # (b) constructs
    op_rx = KINDS["opfamily"][4]
    op_starts = {m.start() for m in op_rx.finditer(t)}
    for kind, (_, _, _, _, use_rx) in KINDS.items():
        if use_rx is None:
            continue
        for m in use_rx.finditer(t):
            ln = _line_of(t, m.start())
            if _USE_LINE.match(src.code[ln - 1]):
                continue
            name, fl = owner_fn(m.start())
            out.append((kind, ln, name, fl, "use"))
    catalog = in_any(src.path, rk_keysem._CATALOG)
    for rk, pat in rk_keysem.USES:
        for m in pat.finditer(t):
            if m.start() in op_starts or (catalog and rk_keysem._FACTS.match(t, m.start())):
                continue
            ln = _line_of(t, m.start())
            if _USE_LINE.match(src.code[ln - 1]):
                continue
            name, fl = owner_fn(m.start())
            out.append((RK_KIND[rk], ln, name, fl, "use"))
    for i, raw in enumerate(src.lines):
        if i < len(src.code) and '""' in src.code[i] and STRING_VOLATILE.search(raw):
            off = sum(len(x) + 1 for x in src.code[:i])
            name, fl = owner_fn(off)
            out.append(("volatile", i + 1, name, fl, "use"))
    # (c) copy comments
    for i, c in enumerate(src.comments):
        m = COPY.search(c)
        if m and not PG_REF.search(c[m.end():m.end() + 40]):
            off = sum(len(x) + 1 for x in src.code[:i])
            name, fl = owner_fn(off)
            if name == "-":           # a doc comment sits above its function: charge the next one
                after = [(line, n) for n, line, _ in fns if line > i + 1]
                if after:
                    fl, name = min(after)
            out.append(("copy", i + 1, name, fl, "copy"))
    return out


def body_tokens(src, fn_line, fn_name, hit_lines):
    """The normalized token sequence a near-copy test compares."""
    if fn_line == 0:
        lines = sorted({n for h in hit_lines for n in range(h - 1, h + 2)})
    else:
        span = None
        for name, line, (a, b) in fns_of(src):
            if line == fn_line and name == fn_name:
                span = (line, _line_of(src.joined("code"), b))
                break
        if span and span[1] - span[0] <= LONG:
            lines = list(range(span[0], span[1] + 1))
        else:
            lines = sorted({n for h in hit_lines for n in range(max(1, h - WINDOW), h + WINDOW + 1)})
    text = "\n".join(src.code[n - 1] for n in lines if 0 < n <= len(src.code))
    toks = _TOK.findall(text)
    if fn_line and "{" in toks and lines and lines[0] == fn_line:
        toks = toks[toks.index("{"):]        # the body only: a copy keeps its walk and renames its parameters
    out = []
    for k, tk in enumerate(toks):
        if tk == fn_name:
            continue
        if tk.isdigit():
            out.append("0")
        elif tk[0].isalpha() or tk[0] == "_":
            prev = toks[k - 1] if k else ""
            nxt = toks[k + 1] if k + 1 < len(toks) else ""
            if tk in _KW or tk[0].isupper() or prev in (".", "::") or nxt in ("(", "::", "!"):
                out.append(tk)
            else:
                out.append("v")
        else:
            out.append(tk)
    return out


def salient(tokens):
    """The names a body calls, reads or matches on (keywords and normalized locals left out)."""
    return {x for x in tokens if x not in _KW and (x[0].isalpha() or x[0] == "_") and x != "v"}


def cluster(impls):
    """impls: [(key, tokens)] of one kind -> {key: cluster index}, clusters numbered by size (largest first)."""
    n = len(impls)
    parent = list(range(n))

    def find(x):
        while parent[x] != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x
    exact = {}
    for i, (_, tk) in enumerate(impls):
        k = tuple(tk)
        if len(tk) >= 3 and k in exact:
            parent[find(i)] = find(exact[k])
        else:
            exact.setdefault(k, i)
    order = sorted(range(n), key=lambda i: len(impls[i][1]))
    names = [salient(tk) for _, tk in impls]
    for x in range(n):
        i = order[x]
        a = impls[i][1]
        if len(a) < 6:
            continue
        sm = difflib.SequenceMatcher(None, autojunk=False)
        sm.set_seq2(a)                       # seq2 is the one SequenceMatcher caches
        for y in range(x + 1, n):
            j = order[y]
            b = impls[j][1]
            if len(b) * 0.7 > len(a):        # sorted by length: every later body is longer still
                break
            if find(i) == find(j):
                continue
            sm.set_seq1(b)
            if not (sm.real_quick_ratio() >= SIM and sm.quick_ratio() >= SIM and sm.ratio() >= SIM):
                continue
            # two short wrappers can share a shape and nothing else: they also share the names they call or read
            ja = len(names[i] & names[j]) / max(1, len(names[i] | names[j]))
            if ja >= 0.34 or min(len(a), len(b)) >= 40:
                parent[find(j)] = find(i)
    groups = collections.defaultdict(list)
    for i in range(n):
        groups[find(i)].append(i)
    ranked = sorted(groups.values(), key=lambda g: (-len(g), impls[g[0]][0]))
    out = {}
    for c, g in enumerate(ranked, 1):
        for i in g:
            out[impls[i][0]] = c
    return out


def census(root, file_map=None):
    """-> (rows, impls): rows = TSV dicts per hit; impls = {(kind, path, fn, fn_line): {...}}."""
    owners = Owners(file_map)
    fams = families(root)
    srcs, hits = {}, []
    for p in walk_sources(root):
        if not p.startswith(G):
            continue
        t = read(root, p)
        if generated(t):
            continue
        s = Src(p, t)
        found = scan_file(s)
        if found:
            srcs[p] = s
            for kind, line, fn, fl, via in found:
                hits.append((kind, p, line, fn, fl, via))
    # a copy comment takes the kinds its function implements; else it stays `copy`
    fn_kinds = collections.defaultdict(set)
    for kind, p, line, fn, fl, via in hits:
        if kind != "copy":
            fn_kinds[(p, fn, fl)].add(kind)
    expanded = []
    for kind, p, line, fn, fl, via in hits:
        if kind == "copy" and fn_kinds.get((p, fn, fl)):
            expanded += [(k, p, line, fn, fl, via) for k in sorted(fn_kinds[(p, fn, fl)])]
        else:
            expanded.append((kind, p, line, fn, fl, via))
    seen, rows = set(), []
    impls = collections.OrderedDict()
    for kind, p, line, fn, fl, via in sorted(expanded, key=lambda h: (list(KINDS) + ["copy"]).index(h[0])):
        if (kind, p, line, via) in seen:
            continue
        seen.add((kind, p, line, via))
        key = (kind, p, fn, fl)
        im = impls.setdefault(key, {"lines": [], "vias": set(), "owner": owners.of(p),
                                    "home": kind_home(kind, p, fams) if kind != "copy" else "no"})
        im["lines"].append(line)
        im["vias"].add(via)
        rows.append({"kind": kind, "path": p, "line": line, "fn": fn, "fn_line": fl, "via": via})
    for key, im in impls.items():            # only a delegate's name hit: it asks the home and decides nothing itself
        if im["vias"] == {"delegate"} and im["home"] != "yes":
            im["home"] = "delegate"
    by_kind = collections.defaultdict(list)
    for key, im in impls.items():
        kind, p, fn, fl = key
        by_kind[kind].append((key, body_tokens(srcs[p], fl, fn, im["lines"])))
    for kind, lst in by_kind.items():
        for key, c in cluster(lst).items():
            impls[key]["cluster"] = f"{kind}:c{c:03d}"
    for r in rows:
        im = impls[(r["kind"], r["path"], r["fn"], r["fn_line"])]
        r.update(owner=im["owner"], home=im["home"], cluster=im["cluster"],
                 text=srcs[r["path"]].lines[r["line"] - 1].strip()[:160])
    return rows, impls


COLS = ["kind", "path", "line", "fn", "fn_line", "owner", "home", "cluster", "via", "text"]


def write_tsv(path, rows, label):
    with open(path, "w") as fh:
        fh.write(f"# dup-census {label}\n")
        fh.write("\t".join(COLS) + "\n")
        for r in rows:
            fh.write("\t".join(str(r[c]).replace("\t", " ") for c in COLS) + "\n")


def summary_md(impls, label, top=10):
    kinds = list(KINDS) + ["copy"]
    per = collections.OrderedDict((k, {"impl": 0, "home": 0, "parked": 0, "clusters": set(), "dup_clusters": 0}) for k in kinds)
    lanes = collections.defaultdict(collections.Counter)
    members = collections.defaultdict(list)
    names = collections.defaultdict(list)
    delegates = []
    for (kind, p, fn, fl), im in impls.items():
        if im["home"] == "delegate":
            delegates.append((kind, p, fn, fl, im["owner"]))
            continue
        d = per[kind]
        d["impl"] += 1
        d["home"] += im["home"] == "yes"
        d["parked"] += im["home"] == "parked"
        d["clusters"].add(im["cluster"])
        if im["home"] != "yes":
            lanes[im["owner"]][kind] += 1
        members[im["cluster"]].append((p, fn, fl, im["owner"], im["home"]))
        if fn != "-":
            names[(kind, fn)].append((p, im["owner"]))
    for c, ms in members.items():
        if len(ms) > 1:
            per[c.split(":")[0]]["dup_clusters"] += 1
    L = [f"# Duplication census {label}", "",
         "Report-only (crates/gravity/lint/reports/dup_census.py). An implementation is a function deciding the kind itself; "
         "calls of the home's primitive are not counted. Clusters fold near-identical bodies (token similarity >= 0.80). "
         "Target: one implementation per kind, in its home.", "",
         "## Per kind", "", "\"parked\" = moved into the home's directory as a family file (effects/visible/<family>.rs, "
         "typesem/<family>.rs), not yet switched to the primitive.", "",
         "| kind | implementations | home (primitive) | parked | outside | clusters | clusters with 2+ | shared primitive |",
         "|---|---:|---:|---:|---:|---:|---:|---|"]
    tot = [0, 0, 0, 0]
    for k, d in per.items():
        prim = KINDS[k][2] if k in KINDS else "(comments saying the code copies another walk)"
        out = d["impl"] - d["home"] - d["parked"]
        L.append(f"| {k} | {d['impl']} | {d['home']} | {d['parked']} | {out} | {len(d['clusters'])} | {d['dup_clusters']} | {prim} |")
        if k != "copy":
            tot[0] += d["impl"]
            tot[1] += d["home"]
            tot[2] += d["parked"]
            tot[3] += len(d["clusters"])
    L.append(f"| **total (without copy)** | {tot[0]} | {tot[1]} | {tot[2]} | {tot[0] - tot[1] - tot[2]} | {tot[3]} | | |")
    L += ["", "## Outside-home implementations per owner lane", "",
          "| lane | total | " + " | ".join(kinds) + " |", "|---|---:|" + "---:|" * len(kinds)]
    for lane, c in sorted(lanes.items(), key=lambda kv: -sum(kv[1].values())):
        L.append(f"| {lane} | {sum(c.values())} | " + " | ".join(str(c.get(k, 0) or "") for k in kinds) + " |")
    # clusters with a member outside the primitive's home first (the copies a UNIFY deletes), then the home's own pairs
    dups = sorted(((c, ms) for c, ms in members.items() if len(ms) > 1 and not c.startswith("copy:")),
                  key=lambda x: (all(m[4] == "yes" for m in x[1]), -len(x[1]), x[0]))
    L += ["", f"## Top {top} duplicate clusters (near-identical bodies; clusters with a copy outside the home first)", ""]
    for c, ms in dups[:top]:
        own = collections.Counter(m[3] for m in ms)
        L.append(f"- **{c}** ({len(ms)} implementations; owners " + ", ".join(f"{o} {n}" for o, n in own.most_common()) + ")")
        for p, fn, fl, o, h in ms[:12]:
            L.append(f"  - {p[len(G):]}:{fl} `{fn}` ({o}{'' if h == 'no' else ', ' + h})")
        if len(ms) > 12:
            L.append(f"  - ... {len(ms) - 12} more")
    same = sorted(((k, v) for k, v in names.items() if len({p for p, _ in v}) > 1), key=lambda kv: (-len(kv[1]), kv[0]))
    L += ["", "## Same function name in several files (same kind)", ""]
    for (kind, fn), v in same[:25]:
        L.append(f"- {kind} `{fn}` x{len(v)}: " + ", ".join(f"{p[len(G):]} ({o})" for p, o in v[:8]) + (" ..." if len(v) > 8 else ""))
    L += ["", "## Delegates (a short body that only asks the home; not counted, not queued)", ""]
    for kind, p, fn, fl, o in delegates:
        L.append(f"- {kind} {p[len(G):]}:{fl} `{fn}` ({o})")
    copies = [(p, fn, fl, im) for (kind, p, fn, fl), im in impls.items() if "copy" in im["vias"]]
    L += ["", "## Copy comments (\"copy of\", \"same body as\", \"<x>'s walk\")", ""]
    seen = set()
    for p, fn, fl, im in copies:
        for ln in im["lines"]:
            if (p, ln) not in seen:
                seen.add((p, ln))
                L.append(f"- {p[len(G):]}:{ln} `{fn}` ({im['owner']})")
    return "\n".join(L) + "\n"


# ---------------------------------------------------------------------------------------------------------------------

def selftest():
    tmp = tempfile.mkdtemp(prefix="dupcensus-selftest-")
    bad = []
    try:
        files = {
            G + "lowerv4/src/ops/sort/a.rs": "pub fn raise_free(gx: &Gx, n: Node) -> bool {\n    match n.tag() {\n"
            "        Tag::Var => true,\n        Tag::Const => true,\n        Tag::FuncExpr => leak(gx, n.funcid()) && n.args().all(|a| raise_free(gx, a)),\n"
            "        _ => false,\n    }\n}\n",
            G + "lowerv4/src/ops/agg/b.rs": "// sort/a.rs raise_free's walk\npub fn quiet_expr(cx: &Gx, e: Node) -> bool {\n    match e.tag() {\n"
            "        Tag::Var => true,\n        Tag::Const => true,\n        Tag::FuncExpr => leak(cx, e.funcid()) && e.args().all(|x| quiet_expr(cx, x)),\n"
            "        _ => false,\n    }\n}\n",
            G + "lowerv4/src/ops/agg/c.rs": "pub fn user_fn(f: u32) -> bool { f >= FIRST_NORMAL_OID }\n"
            "pub fn acts(c: &Cat, f: u32) -> bool { c.func(f).is_ok_and(|p| p.volatility == Some(b'v')) }\n"
            "pub fn knob() -> bool { std::env::var(\"PGRUST_X\").is_ok() }\n",
            G + "decompose/src/effects/read.rs": "pub fn user_code(funcid: u32) -> bool { funcid >= FIRST_NORMAL_OID }\n",
            G + "lowerv4/src/ops/hashjoin/d.rs": "pub fn hj(gx: &Gx, k: usize) -> bool { effects::visible::calls_visible(gx, k).is_none() }\n",
            G + "decompose/src/effects/hashjoin.rs": "pub(crate) fn own_side(c: &Cat, f: u32) -> bool {\n"
            "    c.func(f).is_ok_and(|p| p.volatility == Some(b'v'))\n}\n",
            G + "lowerv4/src/effects/visible/topn.rs": "pub(crate) fn order_volatile(gx: &Gx, e: Node) -> bool {\n"
            "    gx.catalog.func(e.funcid()).is_ok_and(|x| x.volatility == Some(b'v'))\n}\n"
            "pub(crate) fn sort_volatile(gx: &Gx, e: Node) -> bool {\n    super::calls(gx, e) || crate::effects::volatile(gx, e)\n}\n",
        }
        for p, t in files.items():
            os.makedirs(os.path.dirname(os.path.join(tmp, p)), exist_ok=True)
            open(os.path.join(tmp, p), "w").write(t)
        rows, impls = census(tmp)
        got = {(k, p[len(G):], fn) for (k, p, fn, fl) in impls}
        want = {("raise-free", "lowerv4/src/ops/sort/a.rs", "raise_free"), ("raise-free", "lowerv4/src/ops/agg/b.rs", "quiet_expr"),
                ("user-fn", "lowerv4/src/ops/agg/c.rs", "user_fn"), ("volatile", "lowerv4/src/ops/agg/c.rs", "acts"),
                ("env-read", "lowerv4/src/ops/agg/c.rs", "knob"), ("user-fn", "decompose/src/effects/read.rs", "user_code"),
                ("volatile", "lowerv4/src/effects/visible/topn.rs", "order_volatile"),
                ("volatile", "lowerv4/src/effects/visible/topn.rs", "sort_volatile"),
                ("volatile", "decompose/src/effects/hashjoin.rs", "own_side")}
        for w in sorted(want - got):
            bad.append(f"missed {w}")
        for g in sorted(got - want):
            bad.append(f"unexpected {g}")
        cl = {(k, fn): im["cluster"] for (k, p, fn, fl), im in impls.items()}
        if cl.get(("raise-free", "raise_free")) != cl.get(("raise-free", "quiet_expr")):
            bad.append("the two raise-free walks are not one cluster")
        if cl.get(("user-fn", "user_fn")) == cl.get(("user-fn", "user_code")):
            pass                       # identical one-liners may fold; either way is fine
        homes = {(k, fn): im["home"] for (k, p, fn, fl), im in impls.items()}
        if homes.get(("user-fn", "user_code")) != "yes" or homes.get(("user-fn", "user_fn")) != "no" \
                or homes.get(("volatile", "order_volatile")) != "parked" or homes.get(("volatile", "sort_volatile")) != "delegate":
            bad.append("home flags wrong (decompose effects is home, ops/agg is not)")
        if homes.get(("volatile", "own_side")) != "parked":
            bad.append("a family file beside the effects primitive (decompose/src/effects/hashjoin.rs) is not parked")
        if not any(r["via"] == "copy" and r["fn"] == "quiet_expr" for r in rows):
            bad.append("the copy comment above quiet_expr is not listed")
        md = summary_md(impls, "selftest")
        if "raise-free" not in md or "clusters with 2+" not in md:
            bad.append("summary missing sections")
        for k, p, fn in sorted(got):
            print(f"  {k:10} {p}:{fn}  cluster {cl[(k, fn)]}")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    for b in bad:
        print("SELFTEST FAIL " + b)
    print(f"selftest: {'RED' if bad else 'GREEN'} ({len(bad)} failures)")
    return 1 if bad else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", default=os.path.abspath(os.path.join(HERE, "../../../..")))
    ap.add_argument("--file-map", help="FILE-MAP.tsv (owner lanes); without it every owner is '-'")
    ap.add_argument("--tsv", help="write one row per hit here")
    ap.add_argument("--md", help="write the summary here")
    ap.add_argument("--label", default="", help="a label for the headers (e.g. the trunk sha and UTC time)")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    if a.selftest:
        sys.exit(selftest())
    rows, impls = census(a.root, a.file_map)
    if a.tsv:
        write_tsv(a.tsv, rows, a.label)
    md = summary_md(impls, a.label)
    if a.md:
        open(a.md, "w").write(md)
    else:
        print(md)
    n = collections.Counter(k for (k, p, fn, fl) in impls)
    print("dup-census: " + ", ".join(f"{k} {n[k]}" for k in list(KINDS) + ["copy"]) + f" ({len(rows)} hits)", file=sys.stderr)


if __name__ == "__main__":
    main()
