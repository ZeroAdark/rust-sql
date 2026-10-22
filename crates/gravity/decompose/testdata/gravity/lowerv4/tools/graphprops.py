#!/usr/bin/env python3
"""Kinds and properties of one statement from decompose's graph dump (lane V4-PLAN, M0 reconciliation, 2026-09-23).

The census's side of lowerv4 `props.rs` + `graph.rs`: for every plan fixture with a golden
(`crates/gravity/decompose/tests/golden/<id>.graph`), the kinds the graph needs and the property tokens the road
predicates read come from the graph's lines, the same descriptors props.rs reads, never from the SQL text. Catalog
facts (volatility, signatures, operator procs) come from the reference `.dat` files, tiers and shim names from
`gravity_fn` (tier.rs, shims/catalog.rs), as M0-RIG's catalog answers them.

`classify_graph(text) -> (kinds, props)` or None when decompose took no graph (a refusal / not a graph statement).
"""
import os
import re

import graphprops_merge  # the MergeAppend pull-form mirror (props/merge.rs; split for graphprops.py's budget)
from graphprops_topk import group_row_sort, top_reused, topk_wide  # V4R-126 / D1021: props/topk.rs and graph.rs mirrors
import graphprops_count  # noqa: E402  (props/count.rs's mirror, split for the file budget)
from graphprops_grouped import key_arms
import graphprops_write  # the write line's props (props/ops.rs Op::Write, props.rs triggers; split for the budget)

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", "..", "..", "..", "..", ".."))
GOLDEN = os.path.join(ROOT, "crates/gravity/decompose/tests/golden")
CATDIR = os.path.join(ROOT, "crates/postgres-18.6-reference/src/include/catalog")
FN = os.path.join(ROOT, "crates/gravity/fn/src")

FIRST_NORMAL_OID = 16384
JSON_TYPES = {114, 3802}
# FuncFacts.node_arg: a polymorphic / any / record / internal argument (pg_type: anyelement anyarray anynonarray anyenum
# anyrange anymultirange anycompatible* any record internal)
POLY_TYPES = {2283, 2277, 2776, 3500, 3831, 4537, 5077, 5078, 5079, 5080, 4538, 2276, 2249, 2281}
ELEM_POLY = {2283, 2776, 5077, 5079}
TEXT_TYPES = {25, 1043, 19}
BPCHAR_TYPES = {1042}
BYTES_TYPES = {17, 869, 650}
# V4R-118: the sort key kinds whose (flags, word) pair is the whole key (props/sort.rs lane_key)
LANE_KINDS = {"I32", "I64", "Date", "Timestamp", "F32", "F64"}
NUM_TYPES = {1700}
FIXED_IMAGE = {2950: "Uuid", 1186: "IntervalNorm", 1266: "Timetz"}
FIXED_ORDER = {19: "rep.hosted.order.fixed", 774: "rep.hosted.order.fixed", 829: "rep.hosted.order.fixed"}  # a FixedPtr cell with no native image (props/ops.rs fixed_order; HO: no var copy, rep.hosted.order.fixed)
WORD_KINDS = {"I32", "I64", "Timestamp", "F64", "Date", "F32", "Bool", "U64"}
COUNT_CLASSES = {"CountStar", "CountAny", "CountPair"}
# decompose fold_of's registers with a decided numeric scale (props.rs class_kind, D256): the rest fold in a Var slot
SCALED_REGS = {"SumNum", "AvgNum", "MinNum", "MaxNum"}
LIKE = {"textlike", "namelike", "textnlike", "namenlike", "bpcharlike", "bpcharnlike", "texticlike", "texticnlike",
        "nameiclike", "bpchariclike"}
TEXT_EQ = {"texteq", "textne", "bpchareq", "nameeq", "nameeqtext", "texteqname"}
LENGTH = {"textlen", "textoctetlen", "bpcharlen", "bpcharoctetlen", "byteaoctetlen", "length_in_encoding"}
JOIN_KIND = {"JOIN_INNER": "join.inner", "JOIN_UNIQUE_OUTER": "join.inner", "JOIN_UNIQUE_INNER": "join.inner",
             "JOIN_LEFT": "join.left", "JOIN_SEMI": "join.semi", "JOIN_ANTI": "join.anti", "JOIN_RIGHT": "join.right",
             "JOIN_RIGHT_SEMI": "join.right", "JOIN_RIGHT_ANTI": "join.right", "JOIN_FULL": "join.full"}
BREAKER_KIND = {"PlainAgg": "fold.plain", "HashAgg": "keyed.hashagg", "HashBuild": "join.inner", "Sort": "runs.sort",
                "Materialize": "nest.spool", "Distinct": "keyed.distinct_plain", "TopK": "runs.topk_rows"}
WRITE_KIND = {"INSERT": "write.insert", "UPDATE": "write.update", "MERGE": "write.merge", "DELETE": "write.delete",
              "SELECT": "write.delete"}
DATUM = {"w": "rep.word.datum", "code": "rep.code.datum", "pn": "rep.packed.datum", "vl": "rep.varlena.datum",
         "cs": "rep.varlena.datum", "fx": "rep.fixed.datum"}
ARMS = {  # key_kind's families: word, code, packed, varlena, fixed, hosted
    0: "word", 1: "code", 2: "packed", 3: "varlena", 4: "fixed", 5: "hosted"}


# ---------------------------------------------------------------------------------------------- catalog
def dat(name):
    """One reference `.dat` file's entries as dicts (the rig's parser: quoted values, `#` comment lines skipped)."""
    text = open(os.path.join(CATDIR, name)).read()
    out, cur, key, val, i = [], None, [], None, 0
    while i < len(text):
        c = text[i]
        if val is not None:
            if c == "\\":
                i += 1
                val.append(text[i])
            elif c == "'":
                cur["".join(key)] = "".join(val)
                key, val = [], None
            else:
                val.append(c)
        elif cur is not None:
            if c == "'":
                val = []
            elif c == "}":
                out.append(cur)
                cur = None
            elif c.isalnum() or c == "_":
                key.append(c)
            elif c in ",=>":
                if c == ",":
                    key = []
        elif c == "#":
            while i < len(text) and text[i] != "\n":
                i += 1
        elif c == "{":
            cur, key = {}, []
        i += 1
    return out


class Catalog:
    def __init__(self):
        types = dat("pg_type.dat")
        self.type_oid = {}
        for e in types:
            if "oid" in e and "typname" in e:
                self.type_oid[e["typname"]] = int(e["oid"])
                if "array_type_oid" in e:
                    self.type_oid["_" + e["typname"]] = int(e["array_type_oid"])
        t = lambda n: self.type_oid.get(n, 0)
        self.procs, names = {}, {}
        for e in dat("pg_proc.dat"):
            if "oid" not in e:
                continue
            o = int(e["oid"])
            names.setdefault(e["proname"], o)
            args = e.get("proargtypes", "").split()
            names.setdefault("%s(%s)" % (e["proname"], ",".join(args)), o)
            self.procs[o] = {"name": e["proname"], "vol": e.get("provolatile", "i"), "ret": t(e.get("prorettype", "")),
                             "args": [t(a) for a in args], "lang": e.get("prolang", "internal")}
        self.ops = {}
        for e in dat("pg_operator.dat"):
            if "oid" not in e:
                continue
            code = e.get("oprcode", "")
            self.ops[int(e["oid"])] = {"code": names.get(code, 0), "left": t(e.get("oprleft", "")),
                                       "right": t(e.get("oprright", "")), "ret": t(e.get("oprresult", ""))}
        self.tier = {int(o): tr for o, tr in re.findall(r"Row \{ oid: (\d+), name: \"[^\"]*\", tier: Tier::(\w)",
                                                           open(os.path.join(FN, "tier.rs")).read())}
        self.shim = {int(o): n for o, n in re.findall(r"Shim \{ oid: (\d+), name: \"([^\"]*)\"",
                                                        open(os.path.join(FN, "shims/catalog.rs")).read())}

    def opcode(self, opno):
        return self.ops.get(opno, {}).get("code", 0)


_CAT = None


def catalog():
    global _CAT
    if _CAT is None:
        _CAT = Catalog()
    return _CAT


# ---------------------------------------------------------------------------------------------- expressions
def final_order(a):
    """props/ops.rs final_order: the order kind a top over groups needs for aggregate final `a` as its sort key (None: a
    final topk_groups.rs orders at M1: a count, an i64 sum, an exact integer avg, an integer / text min / max, D358)."""
    if a is None:
        return None
    c = a["cls"]
    ext = c[4:-1] if c.startswith(("Min(", "Max(")) else None
    if c in COUNT_CLASSES or c in ("SumI64", "AvgI128") or ext in ("Int", "U64", "Text"):
        return None
    if c in ("SumF32", "SumF64", "AvgF64", "SumMoney", "BoolAnd", "BoolOr") or c.startswith(("Stat3F64", "Stat6F64", "BitI64")) \
            or ext in ("F64", "Tid"):
        return "rep.word.order"
    if c in ("SumI128", "SumNum", "AvgNum") or c.startswith(("Stat3I128", "Stat3Num")) or ext == "Num":
        return "rep.packed.order"
    if c in ("SumInterval", "AvgInterval"):
        return "rep.fixed.order"
    if ext == "Bytes" or c.startswith("BitVar"):
        return "rep.varlena.order"
    return "rep.hosted.order"


class E:
    """One node of a dumped expression: head (`op`, `fn`, `agg`, `case`, `var`, `const`, `param`, `te`, `tag` …),
    oid (the call's), kids, and for leaves the text (`v1.3`, `c25:'x'`, `$exec0`)."""
    __slots__ = ("head", "oid", "kids", "text", "extra")

    def __init__(self, head, oid=0, kids=(), text="", extra=None):
        self.head, self.oid, self.kids, self.text, self.extra = head, oid, list(kids), text, extra

    def walk(self):
        yield self
        for k in self.kids:
            yield from k.walk()


CALL = re.compile(r"(op|fn|distinct|nullif|any|all|win|relabel|io|subplan)(\d+)\(")
WORD = re.compile(r"(and|or|not|isnull|notnull|coalesce|minmax|array)\(")
AGG = re.compile(r"agg(\d+)(\*)?(!distinct)?\(")
VAR = re.compile(r"(v(\d+)|outer|inner|index)\.(-?\d+)")
CONST = re.compile(r"c(\d+):")
PARAM = re.compile(r"\$(exec)?(\d+)")
TE = re.compile(r"(\d+)(j?)=")
TAG = re.compile(r"T_\w+")
ENDERS = (")", ",", "]", " then ", " else ", " filter ")
SRF_FN = re.compile(r"\[\d+:(\w+)(?: \{[^}]*\})? fn(\d+)(?: cols=\d+)? ")


class Parser:
    def __init__(self, s):
        self.s, self.i = s, 0

    def peek(self, t):
        return self.s.startswith(t, self.i)

    def eat(self, t):
        if not self.peek(t):
            raise ValueError("expected %r at %d in %r" % (t, self.i, self.s[:200]))
        self.i += len(t)

    def args(self):
        self.eat("(")
        out = []
        while not self.peek(")"):
            out.append(self.expr())
            if self.peek(", "):
                self.i += 2
            elif not self.peek(")"):
                raise ValueError("bad arg list at %d in %r" % (self.i, self.s[:200]))
        self.eat(")")
        return out

    def const(self, typ):
        s, i = self.s, self.i
        if s.startswith("'", i):
            j = i + 1
            while True:  # a quote ends the literal where an argument separator follows it (bytes print raw)
                j = s.index("'", j)
                if j + 1 >= len(s) or any(s.startswith(e, j + 1) for e in ENDERS):
                    break
                j += 1
            self.i = j + 1
            return E("const", typ, text=s[i:j + 1])
        m = re.compile(r"null|-?\d+|#-?\d+|<varlena>").match(s, i)  # #<constlen>: -2 a cstring / unknown literal
        if not m:
            raise ValueError("bad const at %d in %r" % (i, s[:200]))
        self.i = m.end()
        return E("const", typ, text=m.group(0))

    def expr(self):
        s, i = self.s, self.i
        m = re.compile(r"\$exec(\d+)=").match(s, i)  # a subplan's param assignment: read as a target entry
        if m:
            self.i = m.end()
            return E("te", int(m.group(1)), [self.expr()], extra=False)
        m = TE.match(s, i)
        if m and not VAR.match(s, i):
            self.i = m.end()
            return E("te", int(m.group(1)), [self.expr()], extra=bool(m.group(2)))
        m = AGG.match(s, i)
        if m:
            self.i = m.end() - 1
            kids = self.args()
            e = E("agg", int(m.group(1)), kids, extra={"star": bool(m.group(2)), "distinct": bool(m.group(3))})
            if self.peek(" filter "):
                self.i += len(" filter ")
                e.kids.append(E("aggfilter", 0, [self.expr()]))
            return e
        m = CALL.match(s, i)
        if m:
            self.i = m.end() - 1
            return E(m.group(1), int(m.group(2)), self.args())
        m = WORD.match(s, i)
        if m:
            self.i = m.end() - 1
            return E(m.group(1), 0, self.args())
        if s.startswith("case", i):
            self.i += 4
            kids, arg = [], None
            if self.peek("["):
                self.i += 1
                arg = self.expr()
                self.eat("]")
            self.eat("(")
            whens = []
            while not self.peek(")"):
                self.eat("when ")
                cond = E("cond", 0, [self.expr()]) if not self.peek(" then ") else E("cond")
                self.eat(" then ")
                res = self.expr()
                whens.append((cond, res))
                if self.peek(", "):
                    self.i += 2
            self.eat(")")
            dflt = None
            if self.peek(" else "):
                self.i += 6
                dflt = self.expr()
            if arg is not None:
                kids.append(E("cond", 0, [arg]))
            for c, r in whens:
                kids += [c, r]
            if dflt is not None:
                kids.append(dflt)
            return E("case", 0, kids)
        m = VAR.match(s, i)
        if m:
            self.i = m.end()
            return E("var", text=m.group(0), extra=(m.group(1), int(m.group(3))))
        m = CONST.match(s, i)
        if m:
            self.i = m.end()
            return self.const(int(m.group(1)))
        m = PARAM.match(s, i)
        if m:
            self.i = m.end()
            if self.peek("="):  # a binding `$execN=<value>` (nest / subplan params): the value is the expression
                self.i += 1
                return self.expr()
            return E("param", text=m.group(0))
        m = TAG.match(s, i)
        if m:
            self.i = m.end()
            return E("tag", text=m.group(0))
        raise ValueError("unparsed at %d: %r" % (i, s[i:i + 60]))


def parse(s):
    p = Parser(s.strip())
    e = p.expr()
    return e


def parse_list(s):
    """A dumped node list `(a, b, …)`."""
    s = s.strip()
    if not s.startswith("("):
        return [parse(s)]
    p = Parser(s)
    return p.args()


# ---------------------------------------------------------------------------------------------- graph lines
class Pipe:
    def __init__(self, idx, header, parent=None):
        self.idx, self.parent = idx, parent
        m = re.search(r"node=(-?\d+)", header)
        self.node = int(m.group(1)) if m else 0
        m = re.search(r"end=breaker\(slot=(\d+) (\w+)\)", header)
        self.end = ("breaker", int(m.group(1)), m.group(2)) if m else ("sink",)
        self.consumes, self.admit = [], False
        self.source, self.src_lines, self.ops, self.outs = None, [], [], []
        self.inners = []

    def op_names(self):
        return [o[0] for o in self.ops]


class Graph:
    """The pipelines, breakers, groups, init plans and reruns of one dump. A nest's or nested SubPlan's inner pipeline
    prints inline after its op with no end marker: the outer pipeline's own projections are the ones its `outs` lines
    list (one per projection, same width), so a projection the outs sequence claims ends every open inner block; inside
    blocks an op reading the inner's output (`inner.N` for a nest, `subplanN(…)` for a SubPlan) returns to the parent."""

    def __init__(self, text):
        self.pipes, self.breakers, self.groups, self.init_plans, self.reruns = [], {}, [], [], 0
        self.refused = False
        first = text.split("\n", 1)[0]
        self.fid = first[3:].strip() if first.startswith("-- ") else ""  # the golden's own id (its first line)
        regions, cur_group = [], None
        for line in text.split("\n"):
            if not line or line.startswith("--"):
                continue
            if line.startswith("refused:"):
                self.refused = True
                continue
            m = re.match(r"p(\d+) (.*)", line)
            if m:
                regions.append((int(m.group(1)), m.group(2), []))
                cur_group = None
                continue
            if line.startswith("breaker "):
                m = re.match(r"breaker slot=(\d+) kind=(\w+) .*producer=p(\d+)", line)
                self.breakers[(int(m.group(3)), int(m.group(1)))] = m.group(2)
                regions, cur_group = regions + [None], None
                continue
            if line.startswith("group "):
                m = re.match(r"group slot=(\d+) producer=p(\d+) node=(-?\d+) form=(\w+)", line)
                lay = re.search(r"layout=\[[^\]]*\]->\[([^\]]*)\]", line)
                cur_group = {"slot": int(m.group(1)), "producer": int(m.group(2)), "form": m.group(4),
                             "slots": [x.split("@")[0] for x in lay.group(1).split(",") if x] if lay else [],
                             "set": " set=" in line, "keys": [], "aggs": [], "two_pass": False,
                             "zone_topk": " hints=[ZoneTopK" in line or ", ZoneTopK {" in line}
                self.groups.append(cur_group)
                regions.append(None)
                continue
            if line.startswith("init_plan "):
                m = re.match(r"init_plan id=\d+ pipeline=p(\d+) link=(\w+)(?: param=\S+)?(?: slot=(\d+))?", line)
                self.init_plans.append((int(m.group(1)), m.group(2), m.group(3)))
                cur_group = None
                continue
            if line.startswith("rerun "):  # a recursive union's term set (subplan=-1, RECURSIVE_SET) is setop.recursive's, as props.rs
                self.reruns += " subplan=-1 " not in line + " "
                continue
            if line.startswith(("mapped:", "scan-lanes", "graph ", "param ")):  # param: FACTS-1b F6 (graphprops_facts.py)
                cur_group = None
                continue
            if cur_group is not None and line.startswith("  "):
                self.group_line(cur_group, line[2:])
                continue
            if regions and regions[-1] is not None:
                regions[-1][2].append(line)
        for r in regions:
            if r is not None:
                self.pipes.append(self.region(*r))

    @staticmethod
    def group_line(gr, body):
        if body.startswith("key "):
            m = re.match(r"key \d+ word=\d+ resno=(\d+) kind=(\w+) attno=(-?\d+) type=(\d+)(.*?) expr=(.*)$", body)
            im = re.search(r"image=(\w+)", m.group(5))
            gr["keys"].append({"resno": int(m.group(1)), "kind": m.group(2), "type": int(m.group(4)),
                               "image": im.group(1) if im else None, "expr": parse(m.group(6))})
            graphprops_count.key_extra(gr["keys"][-1], m)  # attno, rep (owner.count's tie keys)
        elif body.startswith("agg "):
            m = re.match(r"agg aggno=(\d+) fn=(\d+) reg=(\S+) .* class=(\S+) trans=.* mods=\[([^\]]*)\] expr=(.*)$", body)
            q = Parser(m.group(6))
            e, filt = q.expr(), None
            if q.peek(" filter="):
                q.i += len(" filter=")
                filt = q.expr()
            order = re.search(r"\border=(\d+)", m.group(5))
            sc = re.search(r" scale=(-?\d+)", body)
            ds = re.search(r" dscale_slot=\d+ dscales=(\S*)", body)
            dscales = [255 if d == "null" else int(d) for d in ds.group(1).split(",") if d] if ds else None
            gr["aggs"].append({"aggno": int(m.group(1)), "fn": int(m.group(2)), "reg": m.group(3), "cls": m.group(4), "expr": e,
                               "filter": filt, "order": int(order.group(1)) if order else 0,
                               "scale": int(sc.group(1)) if sc else None, "dscales": dscales,
                               "spilled": re.search(r" trans=Abi\((SumNumWide|AvgNumWide)\)", body) is not None})
            graphprops_count.agg_extra(gr["aggs"][-1], body)  # slot, final (owner.count's count slot)
        elif body.startswith("two_pass "):
            gr["two_pass"] = True

    @staticmethod
    def top_projects(lines, widths):
        """Which of the region's `project` lines are the outer pipeline's own: the outs widths in order, each inner
        block (opened by an `inner` line) keeping the projections after it until the outs sequence needs one."""
        ev = []  # ("push",) | ("proj", width, line index)
        for i, line in enumerate(lines):
            if line.startswith("  inner "):
                ev.append(("push",))
            elif line.startswith("  project "):
                ev.append(("proj", len(parse_list(line[len("  project "):])), i))
        n, m = len(ev), len(widths)
        # ok[e][j][inside]: events e.. can be assigned with outs pointer j
        from functools import lru_cache

        @lru_cache(maxsize=None)
        def ok(e, j, inside):
            if e == n:
                return j == m
            t = ev[e]
            if t[0] == "push":
                return ok(e + 1, j, True)
            if inside and ok(e + 1, j, True):
                return True
            return j < m and widths[j] == t[1] and ok(e + 1, j + 1, False)

        top, j, inside = set(), 0, False
        if not ok(0, 0, False):
            return None
        for e, t in enumerate(ev):
            if t[0] == "push":
                inside = True
            elif inside and ok(e + 1, j, True):
                pass
            else:
                top.add(t[2])
                j, inside = j + 1, False
        return top

    def region(self, idx, header, lines):
        top = Pipe(idx, header)
        widths = [len(l.split()) - 1 for l in lines if l.startswith("  outs")]
        tops = self.top_projects(lines, widths)
        stack, last = [top], None
        for i, line in enumerate(lines):
            cur = stack[-1]
            if line.startswith("    "):  # a source's or an op's sub-line
                if last == "source":
                    cur.src_lines.append(line.strip())
                elif cur.ops:
                    cur.ops[-1][2].append(line.strip())
                continue
            body = line[2:]
            if body.startswith("inner "):
                q = Pipe(None, body[6:], parent=cur)
                cur.inners.append(q)
                if cur.ops:
                    cur.ops[-1][2].append(("inner", q))
                stack.append(q)
                last = None
                continue
            if body.startswith("outs"):
                top.outs.append([t.split("=", 1)[1] for t in body.split()[1:]])
                stack, last = [top], "outs"
                continue
            if body.startswith("desc"):
                cur.consumes += [(int(a), int(b) if b else None) for a, b in re.findall(r"consumes=slot(\d+)(?:@p(\d+))?", body)]
                cur.admit |= " admitting" in body
                continue
            if body.startswith("nodes"):
                continue
            if body.startswith("source "):
                cur.source = body[7:]
                last = "source"
                continue
            name = body.split(" ", 1)[0]
            if tops is not None and i in tops:
                stack = [top]
            elif tops is not None and name == "project":
                pass  # an inner block's projection
            else:
                join_op = name in ("probe", "merge", "build", "drain", "nest", "memo")
                while len(stack) > 1 and stack[-1].parent.ops and (
                        not join_op and re.search(r"\binner\.\d", body) and stack[-1].parent.ops[-1][0] == "nest"
                        or re.search(r"\bsubplan\d+\(", body) and stack[-1].parent.ops[-1][0] == "subplan"):
                    stack.pop()
            cur = stack[-1]
            cur.ops.append((name, body, []))
            last = "op"
        return top

    def all_pipes(self):
        def rec(p):
            yield p
            for q in p.inners:
                yield from rec(q)
        for p in self.pipes:
            yield from rec(p)


# ---------------------------------------------------------------------------------------------- kinds and props
def lane_family(rep):
    if rep.startswith("code"):
        return "code"
    if rep.startswith("pn"):
        return "pn"
    if rep.startswith("fx"):
        return "fx"
    if rep.startswith("vl"):
        return "vl"
    if rep.startswith("cs"):
        return "cs"
    if re.match(r"w\d", rep):
        return "w"
    return None


def image_of_type(typ, coll=0):
    """The key image decompose gives a type (KeyImage::of), for keys the dump prints without one."""
    if typ in TEXT_TYPES:
        return "Collated" if coll >= FIRST_NORMAL_OID else "Text"
    if typ in BPCHAR_TYPES:
        return "Collated" if coll >= FIRST_NORMAL_OID else "Bpchar"
    if typ in BYTES_TYPES:
        return "Bytes"
    if typ in NUM_TYPES:
        return "NumNormalized"
    if typ in FIXED_IMAGE:
        return FIXED_IMAGE[typ]
    return "ByRef"


def key_arm(kind, image, packed, acc):
    """rep::key_kind: the arm a key keys / orders through."""
    if kind == "Code":
        fam = 1
    elif image == "NumNormalized" and packed and acc == "order":
        fam = 5  # D2426: a wide (not narrow) packed numeric orders by its datum, through rep.hosted.order
    elif image in ("NumMantissa", "NumNormalized"):
        fam = 2 if packed else 5 if acc == "order" else 3  # D771: an unpacked numeric orders through rep.hosted.order
    elif kind == "Fp128" and image in ("Text", "Bpchar", "Bytes", "Collated"):
        fam = 3
    elif image in ("Uuid", "IntervalNorm", "Timetz"):
        fam = 4
    elif kind == "Fp128":
        fam = 5
    else:
        fam = 0
    return "rep.%s.%s" % (ARMS[fam], acc)


class Walk:
    def __init__(self, g):
        self.g, self.cat = g, catalog()
        self.K, self.P = {"expr.core"}, set()
        self.lanes = {}  # rti -> {attno: family}
        self.nests = [0, 0]  # nested loops, of which psql's lookup shape (props.rs catalog_nest)

    # ---- lives and roles (graph.rs)
    def life_of(self, p, slot):
        for s, l in p.consumes:
            if s == slot and l is not None:
                return l
        if p.idx is None:
            return None
        for i in range(p.idx - 1, -1, -1):
            e = self.g.pipes[i].end
            if e[0] == "breaker" and e[1] == slot:
                return i
        return None

    def group_of(self, life):
        if life is None:
            return None
        p = self.g.pipes[life]
        if p.end[0] != "breaker":
            return None
        for gr in self.g.groups:
            if gr["producer"] == life and gr["slot"] == p.end[1]:
                return gr
        return None

    def group_at(self, k):
        """The group whose records pipeline `k` builds: its own, or for an Append arm feeding one aggregate breaker with
        the arms after it, the group the last arm declares (graph.rs group_at)."""
        if k is None or k >= len(self.g.pipes) or self.g.pipes[k].end[0] != "breaker":
            return None
        e = self.g.pipes[k].end
        for j in range(k, len(self.g.pipes)):
            f = self.g.pipes[j].end
            if not (f[0] == "breaker" and f[1] == e[1] and f[2] == e[2]):
                break
            gr = self.group_of(j)
            if gr is not None:
                return gr
        return None

    @staticmethod
    def grouped(gr):
        return any(k["resno"] != 0 for k in gr["keys"])

    @staticmethod
    def keyed(gr):
        return gr["form"] in ("Hashed", "Distinct", "Pass1") or gr["two_pass"] or gr["set"]

    def emits_groups(self, gr):
        return self.keyed(gr) and (self.grouped(gr) or gr["set"])

    def grouping(self, gr):
        return gr["form"] not in ("Plain", "Unique") and (gr["form"] != "Distinct" or self.grouped(gr))

    @staticmethod
    def group_kind(gr):
        f = gr["form"]
        if f == "Plain":
            return "fold.plain"
        if f == "Hashed":
            return "keyed.hashagg"
        if f == "Distinct":
            return "keyed.distinct_grouped" if Walk.grouped(gr) else "keyed.distinct_plain"
        if f in ("Sorted", "Group"):
            return "runs.group"
        if f == "Unique":
            return "runs.unique"
        return "keyed.twopass"

    def drained_group(self, p):
        if not (p.source or "").startswith("breaker"):
            return None
        slot = int(re.search(r"slot=(\d+)", p.source).group(1))
        return self.group_at(self.life_of(p, slot))

    def agg_sort(self, k):
        """props/ops.rs (D771): the group's emit sorts its records (runs.topk_groups, or form G: props/sort.rs group_sort),
        so a final orders through its class's final image; any other sort orders the final's cell through its Rep arm."""
        return k is not None and (self.end_need(k) == "runs.topk_groups" or self.group_sort(k))

    def group_sort(self, k):
        """props/sort.rs group_sort (form G): a full sort that orders the records of the keyed group it drains."""
        pipes, p = self.g.pipes, self.g.pipes[k]
        gr = self.drained_group(p)
        s = next((i for i, (n, _, _) in enumerate(p.ops) if n == "sort"), None)
        if gr is None or p.end[0] != "breaker" or s is None or not (p.source or "").startswith("breaker"):
            return False
        body, slot, br = p.ops[s][1], int(re.search(r"slot=(\d+)", p.source).group(1)), p.end[1]
        reader = next((j for j in range(k + 1, len(pipes)) if (pipes[j].source or "").startswith("breaker")
                       and int(re.search(r"slot=(\d+)", pipes[j].source).group(1)) == br), None)
        again = lambda sl, frm: reader is None or any(pipes[m].end[0] == "breaker" and pipes[m].end[1] == sl for m in range(frm + 1, reader + 1))
        life = self.life_of(p, slot)
        return " form=runs" in body and " presorted=" not in body and self.emits_groups(gr) \
            and all(n in ("filter", "project") for n, _, _ in p.ops[:s]) and not again(slot, k if life is None else life) and not again(br, k)

    def drains_grouping(self, p):
        gr = self.drained_group(p)
        return gr is not None and self.grouping(gr) and group_row_sort(p)  # D1021 (graphprops_topk.py)

    def merged(self, k):
        """Pipeline `k`'s runs are a MergeAppend's: the first later pipeline reading its slot merges it."""
        e = self.g.pipes[k].end
        if e[0] != "breaker":
            return False
        for q in self.g.pipes[k + 1:]:
            merges = any(re.match(r"append merge slot=%d\b" % e[1], b) for _, b, _ in q.ops)
            if merges or re.match(r"breaker slot=%d$" % e[1], q.source or ""):
                return merges
        return False

    def set_role(self, k):
        """Life `k` as a set operation's input: a recursive union's work / next / dedup table, a SetOp's left / right."""
        for q in self.g.pipes:
            for name, body, _ in q.ops:
                m = re.match(r"iterate .*work=slot(\d+) next=slot(\d+)(?:.* dedup=slot(\d+))?", body)
                if m and k in [self.life_of(q, int(x)) for x in m.groups() if x is not None]:
                    return "setop.recursive"
                m = re.match(r"setop \w+ \w+ left=slot(\d+) right=slot(\d+)", body)
                if m and k in (self.life_of(q, int(m.group(1))), self.life_of(q, int(m.group(2)))):
                    return "setop.setop"
        return None

    def merge_inner(self, k):
        """Pipeline `k`'s life is a merge join's inner: a merge op reads its slot (graph.rs merge_inner)."""
        return any(self.life_of(q, int(m.group(1))) == k for q in self.g.pipes for _, body, _ in q.ops
                   for m in [re.match(r"merge slot=(\d+)", body)] if m)

    def spool_kind(self, k):
        if any(ip[0] == k and ip[1] == "Cte" for ip in self.g.init_plans):
            return "nest.cte"
        r = self.set_role(k)
        if r:
            return r
        if any(n == "feed" for n in self.g.pipes[k].op_names()):
            return "src.append"
        if self.merge_inner(k):
            return "merge.join"
        for q in self.g.pipes:
            m = re.match(r"breaker slot=(\d+)$", q.source or "")
            if m and self.life_of(q, int(m.group(1))) == k and "window" in q.op_names():
                return "window.agg"
        for q in self.g.all_pipes():
            for name, body, _ in q.ops:
                m = re.match(r"drain matches=slot(\d+)", body)
                if m and self.life_of(q, int(m.group(1))) == k:
                    return "join.drain"
        return "nest.spool"

    def set_kind(self, k):
        names = self.g.pipes[k].op_names()
        r = self.set_role(k)
        if r:
            return r
        if "identity" in names:
            return "setop.setop"
        if "subplan" in names:
            return "nest.subplan"
        return "keyed.distinct_plain"

    def end_kind(self, p):
        for name, body, _ in p.ops:
            if name == "write":
                verb = body.split()[1]
                if verb != "SELECT":
                    return ("rowop", verb)
        return p.end

    def end_need(self, k):
        p = self.g.pipes[k]
        if any(ip[0] == k and ip[1] != "Cte" for ip in self.g.init_plans):  # M8 L0-g: an InitPlan's cell end
            return "nest.initplan"
        if p.end[0] != "breaker":
            e = self.end_kind(p)
            return WRITE_KIND[e[1]] if e[0] == "rowop" else "end.sink"
        b = p.end[2]
        if b == "Materialize":
            return self.spool_kind(k)
        if b == "TopK" and self.drains_grouping(p) and not top_reused(self, k):
            return "runs.topk_groups"
        if b == "Sort" and self.merge_inner(k):  # D1237: before merged (graph.rs end_need, V4R-398)
            return "merge.join"
        if b == "Sort" and self.merged(k):
            return "runs.mergeappend"
        gr = self.group_at(k)
        if gr is not None:
            return self.group_kind(gr)
        if b == "Distinct":
            return self.set_kind(k)
        return BREAKER_KIND[b]

    def drain_life(self, life):
        if life is None or life >= len(self.g.pipes):
            return "nest.spool"
        p = self.g.pipes[life]
        if p.end[0] == "breaker":
            if p.end[2] == "HashBuild":
                return "join.drain"
            gr = self.group_at(life)
            if gr is not None and self.emits_groups(gr):
                return "keyed.emit_group"
            return self.end_need(life)
        return "end.sink"

    def source_kind(self, p):
        s = p.source or ""
        kind = s.split(" ", 1)[0]
        if kind in ("index", "index-only") and graphprops_merge.merge_pull_arm(self.g.pipes, p):
            return "runs.mergeappend"
        if kind == "breaker":
            slot = int(re.search(r"slot=(\d+)", s).group(1))
            if p.idx is None:  # a nest inner: the life its descriptor reads
                life = next((l for sl, l in p.consumes if sl == slot), None)
                return self.drain_life(life) if life is not None else "nest.spool"
            return self.drain_life(self.life_of(p, slot))
        if kind == "range":
            return "src.pgrc" if "am=pgrc" in s else "src.heap"
        if kind in ("index", "index-only"):  # lowerv4 graph.rs source_kind_of (cards/M2.md L0-h)
            forms = {m.group(1) for ln in p.src_lines if ln.startswith("key ") for m in [re.search(r" form=(\w+) ", ln)] if m}
            if " iam=" in s:
                return "src.index_am"
            if kind == "index-only":
                return "src.index_only"
            if forms & {"any", "amany"}:
                return "src.index_array"
            return "src.index_am" if forms & {"amop", "orderby"} else "src.index"
        return {"bitmap": "src.bitmap", "tid": "src.tid", "sample": "src.sample", "const": "src.result",
                "values": "src.values", "named": "srf.function", "tablefunc": "srf.tablefunc"}.get(kind, "nest.spool")

    @staticmethod
    def step_kind(name, body):
        if name == "filter":
            return "step.filter"
        if name == "gate":
            return "step.gate"
        if name == "project":
            return "step.project"
        if name == "limit":
            return "step.limit"
        if name == "first-of-run":
            return "runs.unique"  # V4R-147 DISTINCT ON (graph.rs step_kind)
        if name == "sort":
            if " presorted=" in body and " form=runs" in body:
                return "runs.incremental"
            return "runs.sort" if " form=runs" in body else "runs.topk_rows"
        if name == "build":
            return "join.inner"
        if name == "probe":
            if re.search(r" catalog keys=", body):
                return "nest.catalog"  # psql's lookup nest as a catalog probe (decompose catalog_lookup, V4R-142)
            return JOIN_KIND[re.search(r"\b(JOIN_\w+)", body).group(1)]
        if name == "drain":
            return "join.drain"
        if name == "merge":
            return "merge.join"
        if name == "nest":
            return "nest.loop"
        if name == "memo":
            return "nest.memo"
        if name == "spool":
            return "nest.spool"
        if name == "window":
            return "window.agg"
        if name == "iterate":
            return "setop.recursive"
        if name in ("setop", "identity"):
            return "setop.setop"
        if name == "subplan":
            return "nest.subplan"
        if name == "append":
            return "runs.mergeappend" if body.startswith("append merge") else "src.append"
        if name == "feed":
            return "src.append"
        if name == "write":
            return WRITE_KIND[body.split()[1]]
        if name == "lock":
            return "write.lock"
        if name == "srf":
            if body.startswith("srf tlist"):
                return "srf.projectset"
            fns = [int(m.group(2)) for m in SRF_FN.finditer(body)]
            return "srf.pl" if fns and all(f >= FIRST_NORMAL_OID for f in fns) else "srf.function"
        raise ValueError("op " + name)

    def op_kinds(self, p, groups, merged=False, recursive=False, merge_inner=False):
        for name, body, sub in p.ops:
            k = self.step_kind(name, body)
            if k == "runs.topk_rows" and groups:
                k = "runs.topk_groups"
            k = "runs.mergeappend" if k == "src.append" and merged else k
            k = "merge.join" if k == "nest.spool" and merge_inner else k  # graph.rs pipe_kind (V4R-398)
            if k == "setop.setop" and recursive:  # an identity op filling a recursive union's dedup table
                k = "setop.recursive"
            self.K.add(k)
            if name == "sort" and " presorted=" in body and " form=runs" not in body:
                self.K.add("runs.incremental")
            for x in sub:
                if isinstance(x, tuple) and x[0] == "inner":
                    q = x[1]
                    self.K.add(self.source_kind(q))
                    self.op_kinds(q, False)

    # ---- the walk (props.rs)
    def run(self):
        g = self.g
        for p in g.all_pipes():
            self.note_lanes(p)
        for k, p in enumerate(g.pipes):
            self.K.add(self.source_kind(p))
            self.op_kinds(p, self.drains_grouping(p), self.merged(k), self.end_need(k) == "setop.recursive", self.end_need(k) == "merge.join")
            self.K |= {"nest.spool"} if not self.merged(k) and any(n == "feed" or b.startswith("append union") for n, b, _ in p.ops) else set()  # D455
            self.K.add(self.end_need(k))
            self.source(p)
            gr = self.drained_group(p)
            self.ops(p, gr)
            if p.admit:
                self.K.add("keyed.twopass")
            if self.end_kind(p)[0] in ("sink", "rowop") and p.outs:
                for rep in p.outs[-1]:
                    f = lane_family(rep)
                    if f:
                        self.K.add(DATUM[f])
        for gr in g.groups:
            self.group(gr)
        if g.init_plans:
            self.K.add("nest.initplan")
        if any(ip[1] == "Cte" for ip in g.init_plans):
            self.K.add("nest.cte")
        if g.reruns:
            self.K.add("nest.rerun")
        if self.K & {"class.num_packed", "class.num_varlena", "class.num_wide"}:
            self.P.add("numeric")
        if self.nests == [1, 1]:
            self.P.add("catalog_nest")
        rows = {"src.heap", "src.pgrc", "src.index", "src.bitmap", "src.index_only", "src.index_array", "src.index_am", "src.tid",
                "src.sample", "src.append", "srf.function"}
        if self.K & {"src.result", "src.values"} and not self.K & rows:
            self.P.add("values_only")
        return self.K, self.P

    def note_lanes(self, p):
        s = p.source or ""
        m = re.search(r"\brti=(\d+)", s)
        if not m:
            return
        rti = int(m.group(1))
        for ln in p.src_lines:
            if ln.startswith("lanes"):
                for t in ln.split()[1:]:
                    lm = re.match(r"!?(?:Attr\((\d+)\)|(\w+)):[a-z]+\d*:([^:]+):", t)
                    if lm and lm.group(1):
                        self.lanes.setdefault(rti, {})[int(lm.group(1))] = lane_family(lm.group(3))

    def source(self, p):
        s = p.source or ""
        kind = s.split(" ", 1)[0]
        if kind == "range":
            self.P.add("seq")
        for ln in p.src_lines:
            if kind in ("index", "index-only") and ln.startswith("key ") and re.search(r" form=(any|amany) ", ln):
                self.K.add("src.index_array")
            if kind == "bitmap" and re.match(r"bitmap (and|or)\b", ln):
                self.K.add("src.bitmap_tree")
            # a btree walk's recheck is PostgreSQL's EvalPlanQual-only indexqualorig (props/ops.rs): not compiled, not walked;
            # the dump prints ` iam=` for another AM, whose lossy recheck is
            if ln.startswith("recheck ") and kind in ("index", "index-only") and " iam=" not in s:
                continue
            # a TID clause is a key word the bind evaluates (props/ops.rs, D392): its quals are not walked
            if ln.startswith("recheck "):
                for e in parse_list(ln.split(" ", 1)[1]):
                    self.expr(e)
            if ln.startswith("row "):
                for e in parse_list(ln[4:]):
                    self.expr(e)
        if kind in ("range", "index", "index-only", "bitmap", "tid", "sample"):
            self.P.add("pgrc" if "am=pgrc" in s else "heap")
            for ln in p.src_lines:
                if ln.startswith("lanes"):
                    for t in ln.split()[1:]:
                        lm = re.match(r"!?[^:]+:[a-z]+\d*:([^:]+):", t)
                        f = lane_family(lm.group(1)) if lm else None
                        if f:
                            self.K.add(DATUM[f])
                            if f == "code":
                                self.P.add("code")
                            if f == "pn":
                                self.P.add("packed")

    def ops(self, p, group):
        joins = sum(1 for n in p.op_names() if n in ("probe", "nest", "memo", "merge"))
        if joins >= 2:
            self.K.add("join.chain")
        inp = None
        for name, body, sub in p.ops:
            self.op(name, body, sub, p, group, inp)
            if name == "project":
                inp = parse_list(body[len("project "):])
            catalog = name == "probe" and re.search(r" catalog keys=", body) is not None
            if name == "nest" or catalog:  # props.rs: a Nest, or psql's lookup as a catalog probe (V4R-142)
                self.nests = [self.nests[0] + 1, self.nests[1] + catalog]
            for x in sub:
                if isinstance(x, tuple) and x[0] == "inner":
                    self.source(x[1])
                    self.ops(x[1], None)

    def entry(self, tlist, resno):
        for t in tlist or []:
            if t.head == "te" and t.oid == resno:
                return t.kids[0]
        return None

    def op(self, name, body, sub, p, group, inp):
        packed = "packed" in self.P
        if name == "filter":
            self.P.add("having" if group is not None else "filter")
            self.expr(parse(body[len("filter "):]))
        elif name == "gate":
            rest = body[len("gate "):]
            if rest != "T_List":
                self.expr(parse(rest))
        elif name == "project":
            for e in parse_list(body[len("project "):]):
                self.expr(e)
        elif name == "first-of-run":
            # V4R-147 (props/ops.rs Op::FirstOfRun): the road's predicate, and each key's hash arm by its type, unpacked
            self.P.add("distinct_on")
            for km in re.finditer(r"(\d+):(\d+):(\w+):(\d+)", re.search(r"keys=(\S*)", body).group(1)):
                self.K.add(key_arm(km.group(3), image_of_type(int(km.group(4))), False, "hash"))
        elif name == "sort":
            if " form=zonewalk" in body:
                self.P.add("walk")
            keys = re.search(r"keys=(\S+)", body).group(1)
            # V4R-118 (props/sort.rs full_sort / lane_key / declared_order): a full sort over rows, its first key one word
            full = " form=runs" in body and " presorted=" not in body and group is None
            lane = " form=runs" in body and group is None  # V4R-137 (D479): an incremental sort takes the key lane too
            k0 = re.match(r"\d+[-+]n?:(\w+)(?:/(\w+))?(?:\.\d+)?(?:@(\d+))?", keys)  # V4R-507: or a text / bpchar first 8 bytes under none / C / POSIX
            if lane and k0 and (k0.group(1) in LANE_KINDS and k0.group(2) in (None, "F32", "F64") or k0.group(1) == "Fp128" and k0.group(2) in (None, "Bpchar") and int(k0.group(3) or 0) in (0, 950, 951)):
                self.P.add("lane_key")
            self.P |= {"topk_wide"} if topk_wide(p, body, inp) else set()
            graphprops_count.mark(self, p, group, inp, body)  # V4R-113: the count_order prop
            first = True
            for km in re.finditer(r"(\d+)[-+]n?:(\w+)(?:/(\w+))?(?:\.\d+)?(?:@(\d+))?(#19)?", keys):
                resno, kind, image, coll = int(km.group(1)), km.group(2), km.group(3), int(km.group(4) or 0)
                image = ("Collated" if coll >= FIRST_NORMAL_OID else "Text") if image is None and kind == "Fp128" else image
                if kind == "Fp128":
                    self.P.add("fp_sortkey")
                e = self.entry(inp, resno)
                if first and e is not None:
                    if e.head in ("fn", "op"):
                        self.P.add("order_expr")
                first = False
                if group is not None and e is not None and e.head == "agg" and self.agg_sort(p.idx):  # D771
                    k = final_order(self.agg_of(group, e))  # a final's order kind (props/ops.rs final_order)
                    if k:
                        self.K.add(k)
                    continue
                code = e is not None and self.code_cell(p, group, e)
                code_arm = "rep.varlena.order" if full else "rep.code.order"  # V4R-118: a full sort orders text across parts
                self.K.add(code_arm if code else "rep.word.order" if km.group(5) and coll in (0, 950, 951) else FIXED_ORDER.get(e is not None and self.type_of(e)) or key_arm(kind, image, packed, "order"))  # O3: a name key (#19) under C / POSIX; FIXED_ORDER (V4R-138)
        elif name == "probe":
            decls = re.search(r"decls=(\S+)", body).group(1)
            for dm in re.finditer(r"(\d+)/(\d+):(\w+)(?:\.\d+)?(?:@(\d+))?", decls):
                typ, kind, coll = int(dm.group(1)), dm.group(3), int(dm.group(4) or 0)
                kind = "Fp128" if kind == "none" else kind
                image = image_of_type(typ, coll) if kind == "Fp128" else None
                self.K.add(key_arm(kind, image, packed, "hash"))
            self.clauses(body)
        elif name == "merge":
            self.K.add(key_arm("I64", None, packed, "order"))
            self.clauses(body)
        elif name == "nest":
            if not re.search(r"params=\(\)", body):
                self.K.add("nest.param")
            self.clauses(body)
        elif name == "memo":
            self.K.add(key_arm("I64", None, packed, "hash"))
            m = re.search(r"keys=(\(.*\))", body)
            for e in parse_list(m.group(1)) if m else []:
                self.expr(e)
        elif name in ("setop", "identity"):
            keys = re.search(r"keys=(\S*)", body).group(1)
            for km in re.finditer(r"(\d+)=(\d+):(\w+)", keys):
                self.K.add(self.eq_key_arm(int(km.group(2)), km.group(3), packed, "hash"))
        elif name == "window":
            for part in ("partition", "order"):
                keys = re.search(part + r"=(\S*)", body).group(1)
                for km in re.finditer(r"(\d+)=(\d+):(\w+)", keys):
                    self.K.add(self.eq_key_arm(int(km.group(2)), km.group(3), packed, "order"))
            for x in sub:
                if isinstance(x, str) and x.startswith("fn "):
                    self.expr(parse(x.split(", ", 1)[1] if ", " in x else x))
        elif name == "subplan":
            m = re.search(r" params=\(", body)
            if m:
                j, depth = m.end() - 1, 0
                for j in range(m.end() - 1, len(body)):  # the balanced list: a param may hold "(...)" and " filter "
                    depth += {"(": 1, ")": -1}.get(body[j], 0)
                    if depth == 0:
                        break
                for e in parse_list(body[m.end() - 1:j + 1]):
                    if e.head == "te":
                        e = e.kids[0]
                    self.expr(e)
            m = re.search(r" test=(.*)$", body)
            if m:
                self.expr(parse(m.group(1)))
        elif name == "append":
            if " prune=" in body:
                self.K.add("src.prune")
            m = re.match(r"append merge .* keys=(\S+)", body)
            if m:
                for km in re.finditer(r"(\d+)[-+]n?:(\w+)(?:/(\w+))?(?:\.\d+)?(?:@(\d+))?", m.group(1)):
                    kind, image, coll = km.group(2), km.group(3), int(km.group(4) or 0)
                    if image is None and kind == "Fp128":
                        image = "Collated" if coll >= FIRST_NORMAL_OID else "Text"
                    self.K.add(key_arm(kind, image, packed, "order"))
        elif name == "write":
            graphprops_write.write(self, body)  # multi_target / route / row_trigger / write.transition (split for the budget)
            if " returning" in body:
                self.K.add("write.returning")
            if " on_conflict=" in body:
                self.K.add("write.onconflict")
                m = re.search(r" excl_rti=\d+ cols=\([^)]*\) set=", body)
                if m:
                    q = Parser(body)
                    q.i = m.end()
                    for e in q.args():
                        self.expr(e)
        elif name == "srf":
            for fm in SRF_FN.finditer(body):
                # V4R-233 (M5 Decision 23): srf.pl by language (SQL / PL), FROM and target list; a user function has no
                # .dat row and reads as PL (a C-extension SRF where props disagrees: placement-overrides.tsv)
                pr = self.cat.procs.get(int(fm.group(2)))
                if (pr["lang"] not in ("internal", "c")) if pr else (int(fm.group(2)) >= FIRST_NORMAL_OID):
                    self.K.add("srf.pl")
                q = Parser(body)
                q.i = fm.end()
                node = q.expr()
                if self.json_call(node):
                    self.P.add("jsonb")
                # a native SRF kind is the source's own loop; only a scalar / other function (or a folded FROM
                # expression) is a hosted call
                for x in ([node] if fm.group(1) in ("Scalar", "Other", "Expr") else node.kids):
                    self.expr(x)

    def clauses(self, body):
        for key in (" clauses=", " qual="):
            i = body.find(key)
            if i < 0:
                continue
            p = Parser(body)
            p.i = i + len(key)
            for e in p.args():
                self.expr(e)

    def eq_key_arm(self, opno, kind, packed, acc):
        if kind != "Fp128":
            return key_arm(kind, None, packed, acc)
        typ = self.cat.ops.get(opno, {}).get("left", 0)
        return key_arm(kind, image_of_type(typ), packed, acc)

    @staticmethod
    def agg_of(group, e):
        """The group's aggregate an `agg` node of a sort list is: same function and same dumped tree, else same function."""
        sig = lambda x: (x.head, x.oid, x.text, tuple(sig(k) for k in x.kids))  # noqa: E731
        same = [a for a in group["aggs"] if a["fn"] == e.oid]
        return next((a for a in same if sig(a["expr"]) == sig(e)), same[0] if same else None)

    def var_family(self, v, p):
        """The lane family a Var of pipeline `p`'s relation source reads (None: not a lane of it)."""
        s = p.source or ""
        m = re.search(r"\brti=(\d+)", s)
        base, att = v.extra
        if not m or not base.startswith("v") or int(base[1:]) != int(m.group(1)):
            return None
        return self.lanes.get(int(m.group(1)), {}).get(att)

    def last_project(self, p):
        """Pipeline `p`'s last projection (a group producer's: its keys read it through outer.N), or None."""
        projs = [b for n, b, _ in p.ops if n == "project"]
        return parse_list(projs[-1][len("project "):]) if projs else None

    def code_expr(self, p, e, outer=None):
        """A key read by code: exactly one code lane of `p`'s relation source is read, no VOLATILE call, no aggregate /
        window / subplan / param; any other lane is a word lane read only in a CASE condition (KEY_CODE_IF). `outer` is
        `p`'s last projection when `e` is a group key over it (props/code.rs code_expr): outer.N reads that projection's
        entry N, one level only (an outer.N inside the entry fails)."""
        code, ok = set(), True

        def rec(x, in_cond, in_entry=False):
            nonlocal ok
            if x.head == "var" and x.extra[0] == "outer" and not in_entry:
                t = self.entry(outer, x.extra[1]) if outer is not None else None
                if t is None:
                    ok = False
                else:
                    rec(t, in_cond, True)
                return
            if x.head == "var":
                f = self.var_family(x, p)
                if f == "code":
                    if in_cond:
                        ok = False
                    code.add(x.extra[1])
                elif not (in_cond and f == "w"):
                    ok = False
            elif x.head in ("agg", "win", "subplan", "param"):
                ok = False
            elif x.head == "fn" and self.cat.procs.get(x.oid, {}).get("vol", "v") == "v":
                ok = False
            for kid in x.kids:
                rec(kid, in_cond or x.head == "cond", in_entry)

        rec(e, False)
        return ok and len(code) == 1

    def code_cell(self, p, group, e):
        if group is None:
            return self.code_expr(p, e)
        if e.head != "var" or e.extra[0] != "outer":
            return False
        producer = self.g.pipes[group["producer"]]
        return any(k["resno"] != 0 and k["resno"] == e.extra[1] and self.code_expr(producer, k["expr"], self.last_project(producer))
                   for k in group["keys"])

    def group(self, gr):
        self.K.add(self.group_kind(gr))
        if gr["form"] == "Unique" and self.distinct_on(gr):
            self.P.add("distinct_on")
        keyed = self.keyed(gr)
        if gr["two_pass"]:
            self.K.add("keyed.twopass")
        if gr["set"]:
            self.K.add("keyed.gsets")
        if self.emits_groups(gr):
            self.K.add("keyed.emit_group")
        if gr["zone_topk"]:
            self.P.add("walk")  # V4R-8: an unordered grouped LIMIT the planner walks (ZoneTopK), like a zonewalk sort
        packed, code = "packed" in self.P, "code" in self.P
        producer = self.g.pipes[gr["producer"]] if gr["producer"] < len(self.g.pipes) else None
        for key in gr["keys"]:
            e = key["expr"]
            key_arms(self, gr, key, producer, packed, keyed)   # its hash (and form K's order) arm: graphprops_grouped.py (V4R-128)
            if e.head != "var":
                self.P.add("expr_key")
                if self.named(e, ("mod", "div")):
                    self.P.add("mod_key")
            self.expr(e)
        for a in gr["aggs"]:
            if a["filter"] is not None:
                self.K.add("fold.filter")
                self.expr(a["filter"])
            c = a["cls"]
            if a["order"] > 0 and not c.startswith(("OrderedSet", "Hypothetical")):
                self.K.add("runs.group")  # an ORDER BY inside the call: refused to runs.group (props/group.rs, D346)
            if c in COUNT_CLASSES:
                self.P.add("count")
                kind = "class.word"
            elif c in ("SumI64", "SumI128", "AvgI128", "BoolAnd", "BoolOr") or c.startswith("BitI64"):
                kind = "class.word"
            elif c in ("SumF32", "SumF64", "AvgF64"):
                kind = "class.float"
            elif c in ("SumNum", "AvgNum"):  # V4R-38: the i256 registers are class.num_wide's
                kind = "class.num_wide" if a["reg"] in ("SumNumWide", "AvgNumWide") else "class.num_packed" if a["reg"] in SCALED_REGS else "class.num_varlena"
            elif c.startswith(("Min(", "Max(")):
                kind = self.minmax(c[4:-1], a["expr"], a["reg"])
            elif c.startswith("Collect"):
                kind = "class.collect"
            elif c == "User":
                kind = "class.user"
            elif c.startswith(("Stat3", "Stat6")):
                kind = "class.stats"
            elif c.startswith(("OrderedSet", "Hypothetical")):
                kind = "class.ordered"
            else:
                kind = "class.other"
            if kind == "class.num_packed" and self.wide_packed(gr, a):
                # D396: fold/class_of.rs class_in's wide case (props/code.rs fold_kind); D830: a spilled one's datum folds
                # in class.num_wide, with a dscale companion in class.num_packed (V4R-549; fold/num_carry.rs datum)
                kind = "class.num_varlena" if not a["spilled"] else "class.num_wide" if a["dscales"] is None else "class.num_packed"
            self.K.add(kind)
            self.expr(a["expr"])

    def wide_packed(self, gr, a):
        """props/code.rs fold_kind (D396): a packed numeric aggregate whose argument's tree passes one mantissa word is
        class.num_varlena's, unless a plain, Hashed or Sorted end's num.window tree takes it (fold/plain.rs of_agg; V4R-38;
        a Sorted end folds trees when one of its aggregates has one, num_keyed.rs `forms`: this one's, when its tree takes it)."""
        args = [k for k in a["expr"].kids if k.head == "te"]
        if len(args) != 1:
            return False
        tm = agg_var_facts(self.g.fid, a["fn"], a["aggno"])
        e = args[0].kids[0]
        t = self.num_tree(e, False, True, tm)
        if t is None or t["digits"] <= NUM_NARROW:
            return False
        trees = (gr["form"] == "Plain" and not self.keyed(gr)) or gr["form"] in ("Hashed", "Sorted")
        return not (trees and self.num_of_agg(a, e, tm))

    def num_of_agg(self, a, e, tm):
        if a["reg"] not in ("SumNum", "AvgNum"):
            return False
        t = self.num_tree(e, True, True, tm)
        if t is None or a["scale"] is None or t["scale"] != a["scale"] or not 0 <= a["scale"] <= 18:
            return False
        ds = a["dscales"] is not None
        if t["case"] is not None:
            arms, mixed = t["case"]
            if ds:
                d = a["dscales"] + [255] * (16 - len(a["dscales"]))
                return len(arms) <= 16 and all((255 if x is None else x) == y for x, y in zip(arms, d))
            return not mixed
        return not ds

    def num_tree(self, e, strict, root, tm):
        """fold/num_tree.rs tree (+ num_case.rs compile): {scale, digits, mul_one, case} or None."""
        leaf = lambda sc, dg: {"scale": sc, "digits": dg, "mul_one": True, "case": None}  # noqa: E731
        bounded = lambda t: t["mul_one"] and t["digits"] <= 37  # noqa: E731
        if e.head == "relabel":
            return self.num_tree(e.kids[0], strict, root, tm)
        if e.head == "case":
            kids = list(e.kids)
            if len(kids) >= 2 and kids[1].head == "cond":
                return None  # a simple CASE (an arg)
            pairs, dflt = kids, None
            if len(kids) % 2 == 1:
                pairs, dflt = kids[:-1], kids[-1]
            results = []
            for i in range(0, len(pairs), 2):
                if not pairs[i].kids:
                    return None
                results.append(pairs[i + 1])
            results.append(dflt)
            arms = []
            for r in results:
                if r is None or num_null(r):
                    arms.append(None)
                    continue
                t = self.num_tree(r, strict, False, tm)
                if t is None:
                    return None
                arms.append(t)
            live = [t for t in arms if t is not None]
            if not live:
                return None
            sc = max(t["scale"] for t in live)
            dg = max(t["digits"] + (sc - t["scale"]) for t in live)
            mixed = any(t["scale"] != sc for t in live)
            if mixed and not root:
                return None
            t = {"scale": sc, "digits": dg, "mul_one": True, "case": ([None if x is None else x["scale"] for x in arms], mixed)}
            return t if bounded(t) or not strict else None
        if e.head == "var":
            f = num_bare_var(e, tm)
            if f is None:
                return None
            vt, tmod = f
            if tmod < 4:
                return None
            x = tmod - 4
            sc, dg = x & 0xFFFF, x >> 16
            return leaf(sc, dg) if vt == 1700 and sc <= 18 and (dg <= NUM_NARROW or not strict) else None
        if e.head == "const":
            return num_const(e, leaf)
        if e.head == "fn":
            if e.oid not in (1740, 1781, 1782) or len(e.kids) != 1:
                return None
            a = e.kids[0]
            if a.head == "const":
                return num_const(a, leaf)
            if a.head != "var":
                return None
            f = num_bare_var(a, tm)
            if f is None or f[0] not in (21, 23, 20):
                return None
            return leaf(0, {21: 5, 23: 10, 20: 19}[f[0]])
        if e.head != "op" or len(e.kids) != 2:
            return None
        x, y = self.num_tree(e.kids[0], strict, False, tm), self.num_tree(e.kids[1], strict, False, tm)
        if x is None or y is None:
            return None
        fid = self.cat.opcode(e.oid)
        if fid == 1726:
            t = {"scale": x["scale"] + y["scale"], "digits": x["digits"] + y["digits"],
                 "mul_one": x["digits"] <= NUM_NARROW or y["digits"] <= NUM_NARROW, "case": None}
        elif fid in (1724, 1725):
            sc = max(x["scale"], y["scale"])
            t = leaf(sc, max(x["digits"] + sc - x["scale"], y["digits"] + sc - y["scale"]) + 1)
        else:
            return None
        return t if bounded(t) or not strict else None

    def minmax(self, x, agg, reg):
        args = [k for k in agg.kids if k.head == "te"]
        if args and args[0].kids[0].head in ("fn", "op"):
            self.P.add("minmax_expr")
        if x in ("Int", "U64"):
            return "class.word"
        if x == "F64":
            return "class.float"
        if x == "Num":
            return "class.num_packed" if reg in SCALED_REGS else "class.num_varlena"
        if x in ("Text", "Bytes"):
            return "class.minmax_code" if reg in ("MinCode", "MaxCode") else "class.text"
        return "class.other"

    def distinct_on(self, gr):
        p = self.g.pipes[gr["producer"]] if gr["producer"] < len(self.g.pipes) else None
        if p is None:
            return False
        tl = self.last_project(p)
        if tl is None:
            return False
        cols = [t for t in tl if t.head == "te" and not t.extra]
        return len(gr["keys"]) < len(cols)

    def named(self, e, parts):
        for x in e.walk():
            fid = self.cat.opcode(x.oid) if x.head == "op" else x.oid if x.head == "fn" else 0
            if fid and any(pt in self.cat.shim.get(fid, "") for pt in parts):
                return True
        return False

    def expr(self, e):
        for x in e.walk():
            if self.json_call(x):
                self.P.add("jsonb")
            h = x.head
            if h == "case":
                self.P.add("case")
            elif h in ("any", "all"):
                self.K.add("expr.saop")
                self.call(self.cat.opcode(x.oid))
            elif h == "var" and x.extra[1] == 0 and x.extra[0].startswith("v"):
                self.K.add("expr.wholerow")
            elif h == "fn":
                self.call(x.oid)
            elif h in ("op", "distinct", "nullif"):
                self.call(self.cat.opcode(x.oid))
            elif h == "tag" and x.text == "T_NextValueExpr":  # props/expr.rs: nextval_internal, VOLATILE
                self.K.add("expr.volatile")

    def json_call(self, x):
        """A call that takes or returns json / jsonb (props.rs json_call): json when its result is json, or when an
        argument is json and the call is not polymorphic (FuncFacts.node_arg). COALESCE / GREATEST / LEAST count only by
        their result: count(j) is no json call, coalesce(j, '{}'::jsonb) is."""
        c = self.cat
        if x.head in ("fn", "agg", "win"):
            pr = c.procs.get(x.oid)
            if pr is None:
                return False
            if self.type_of(x) in JSON_TYPES:
                return True
            return not any(a in POLY_TYPES for a in pr["args"]) and any(a in JSON_TYPES for a in pr["args"])
        if x.head in ("op", "distinct", "nullif", "any", "all"):
            o = c.ops.get(x.oid)
            return o is not None and bool({o["left"], o["right"], o["ret"]} & JSON_TYPES)
        if x.head in ("coalesce", "greatest", "least"):
            return self.type_of(x) in JSON_TYPES
        return False

    def type_of(self, x):
        """An expression's result type where the graph shows it: a typed constant, a call's declared result (a
        polymorphic element result takes its argument's type), an operator's result; COALESCE / GREATEST / LEAST / CASE
        take their first typed arm (the arms share the result type). None when unknown (a Var's type is not printed)."""
        c = self.cat
        h = x.head
        if h == "const":
            return x.oid or None
        if h == "te" and x.kids:
            return self.type_of(x.kids[0])
        if h in ("fn", "agg", "win"):
            pr = c.procs.get(x.oid)
            if pr is None:
                return None
            if pr["ret"] not in POLY_TYPES:
                return pr["ret"]
            if pr["ret"] in ELEM_POLY:
                for a, k in zip(pr["args"], x.kids):
                    if a in ELEM_POLY:
                        t = self.type_of(k)
                        if t is not None:
                            return t
            return None
        if h == "op":
            o = c.ops.get(x.oid)
            return o["ret"] if o else None
        if h in ("coalesce", "greatest", "least", "case"):
            for k in x.kids:
                if k.head == "cond":
                    continue
                t = self.type_of(k)
                if t is not None:
                    return t
        return None

    def call(self, fid):
        pr = self.cat.procs.get(fid)
        if pr is None:
            self.K.add("expr.volatile")
            self.P.add("hosted")
            return
        if pr["vol"] == "v":
            self.K.add("expr.volatile")
        if self.cat.tier.get(fid, "C") != "A":
            self.P.add("hosted")
        name = self.cat.shim.get(fid)
        if name is None:
            return
        if name in LIKE:
            self.P.add("like")
        if name in TEXT_EQ:
            self.P.add("text_eq")
        if name in LENGTH:
            self.P.add("length")
        if "_trunc" in name or "_part" in name or name.startswith("extract_"):
            self.P.add("datetime")
        if name.startswith("numeric"):
            self.P.add("numeric")


NUM_NARROW = 18
VARNO = {"outer": -2, "inner": -1, "index": -3}


def num_null(r):
    while r.head == "relabel":
        r = r.kids[0]
    return r.head == "const" and r.text == "null"


def num_bare_var(e, tm):
    """step/project.rs bare_var (a row column, not INNER_VAR) with the Var's (vartype, vartypmod) from the plan."""
    while e.head == "relabel":
        e = e.kids[0]
    if e.head != "var":
        return None
    base, att = e.extra
    varno = VARNO[base] if base in VARNO else int(base[1:])
    if att <= 0 or varno == -1:
        return None
    return tm.get((varno, att))


def num_const(c, leaf):
    """fold/num_const.rs const_of over the graph's literal."""
    if c.text == "null":
        return None
    digits = lambda m: len(str(abs(m)))  # noqa: E731
    if c.oid in (20, 21, 23):
        return leaf(0, digits(int(c.text))) if re.fullmatch(r"-?\d+", c.text) else None
    if c.oid != 1700 or not c.text.startswith("'"):
        return None
    raw, body, i = bytearray(), c.text[1:-1], 0
    while i < len(body):
        if body.startswith("\\x", i):
            raw.append(int(body[i + 2:i + 4], 16))
            i += 4
        else:
            raw.append(ord(body[i]))
            i += 1
    if len(raw) < 2:
        return None
    h = raw[0] | raw[1] << 8
    if h & 0xC000 == 0x8000:
        scale, weight = (h & 0x1F80) >> 7, h & 0x3F
        if h & 0x40:
            weight -= 64
        neg, digs = bool(h & 0x2000), raw[2:]
    elif h & 0xC000 == 0xC000:
        return None
    else:
        if len(raw) < 4:
            return None
        scale, neg = h & 0x3FFF, bool(h & 0x4000)
        weight = int.from_bytes(raw[2:4], "little", signed=True)
        digs = raw[4:]
    if scale > 18:
        return None
    ds = [digs[j] | digs[j + 1] << 8 for j in range(0, len(digs) - 1, 2)]
    from fractions import Fraction
    v = sum(Fraction(d) * Fraction(10000) ** (weight - k) for k, d in enumerate(ds))
    m = int(v * 10 ** scale)
    if abs(m) >= 2 ** 63:
        return None
    return leaf(scale, digits(m))


_AGG_FACTS = {}


def agg_var_facts(fid, fn, aggno):
    """The (varno, varattno) -> (vartype, vartypmod) of the Vars in the plan fixture's Aggref(s) `fn` / `aggno`."""
    key = (fid, fn, aggno)
    if key in _AGG_FACTS:
        return _AGG_FACTS[key]
    out = {}
    p = plan_path(fid) if fid else None
    if p and os.path.exists(p):
        text = open(p, errors="replace").read()
        for m in re.finditer(r"\{AGGREF :aggfnoid %d " % fn, text):
            depth, j = 0, m.start()
            while j < len(text):
                if text[j] == "{":
                    depth += 1
                elif text[j] == "}":
                    depth -= 1
                    if depth == 0:
                        break
                j += 1
            blk = text[m.start():j + 1]
            if not re.search(r":aggno %d :" % aggno, blk):
                continue
            for v in re.finditer(r"\{VAR :varno (-?\d+) :varattno (-?\d+) :vartype (\d+) :vartypmod (-?\d+)", blk):
                out.setdefault((int(v.group(1)), int(v.group(2))), (int(v.group(3)), int(v.group(4))))
    _AGG_FACTS[key] = out
    return out


def classify_graph(text):
    g = Graph(text)
    if not g.pipes:
        return None
    return Walk(g).run()


PLANS = os.path.join(ROOT, "crates/gravity/decompose/tests/plans")


def plan_path(fid):
    """A golden id's plan fixture: tests/plans/<fid>.plan, or tests/plans/regress-full/<test>-<nnn>.plan for regress-full-*."""
    p = os.path.join(PLANS, fid + ".plan")
    if fid.startswith("regress-full-") and not os.path.exists(p):
        p = os.path.join(PLANS, "regress-full", fid[len("regress-full-"):] + ".plan")
    return p


def relkind(fid, relid):
    """a relation's relkind from the plan fixture's `-- rel <relid> <name> ... kind=<k>` header line (None: unknown)."""
    p = plan_path(fid)
    if not fid or not os.path.exists(p):
        return None
    with open(p, errors="replace") as fh:
        for line in fh:
            if not line.startswith("-- "):
                break
            m = re.match(r"^-- rel %d \S+ .*\bkind=(\w)" % relid, line)
            if m:
                return m.group(1)
    return None


def golden(fid):
    p = os.path.join(GOLDEN, fid + ".graph")
    return open(p, errors="replace").read() if os.path.exists(p) else None


if __name__ == "__main__":
    import sys
    for fid in sys.argv[1:]:
        r = classify_graph(golden(fid) or "")
        if r is None:
            print(fid, "NOGRAPH")
        else:
            print("%s\t%s\t%s" % (fid, ",".join(sorted(r[0])), ",".join(sorted(r[1]))))
