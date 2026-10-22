#!/usr/bin/env python3
"""The census mirror of decompose's FACTS-1b (M8-plan §7.1; lane M8-FACTS): from each plan fixture (the outfuncs text under
crates/gravity/decompose/tests/plans) and the reference catalog (.dat files), on its own, the facts decompose prints in the
golden graph: F1 `invariant`, F5 `vol=` / `raise-free` on nest / subplan / init_plan lines, F2 `eq=` on nest lines, and F6's
`param` lines (owner, type, typmod, collation; the Rep's storage family against the type's). A golden line that disagrees
fails `--check` (decompose tests/facts_m8b.rs runs it: a mismatch fails ct).

The rules are decompose's (src/rescan.rs, src/join.rs eq_conjunct, src/outs.rs param_reps), written again here:
  - a function / operator outside the builtin catalog is volatile, not strict, not leakproof, in no hash opfamily;
  - raise-free: every call strict and leakproof or on types_core::quiet::call's list (QUIET, effects::quiet_call), or an
    integer % / / by a constant divisor_quiet passes or a privilege function over a privilege text privilege_quiet
    accepts (effects::quiet_args), over a set input collation (effects::collation_set); a row comparison over initdb
    families; an I/O coercion only to a text-like type from a listed source (io_quiet), XMLCONCAT / XMLELEMENT / XMLFOREST
    only over listed value types (xml_quiet), IS JSON only over jsonb or json without UNIQUE KEYS (json_is_quiet); no
    array / domain coercion, subscript, MIN / MAX, other XML / JSON node (a domain
    coercion is VOLATILE too: its CHECKs may call user or VOLATILE code, effects/found.rs DOMAIN_CHECK); a
    scalar / row / MULTIEXPR sublink only over a plan of at most one row; LIMIT / OFFSET bounds non-negative constants; no
    frame offset, row lock, run-time pruning, write or unknown node; a TABLESAMPLE clause only as effects::sample_class passes;
  - invariant: nothing volatile, nothing rescan-unstable (a TABLESAMPLE but a heap BERNOULLI / SYSTEM one, an outer recursion's WorkTableScan, a tuplestore
    scan, LockRows, CURRENT OF, a write, an unknown node) and `allParam` inside the params fixed for the statement (or
    a ModifyTable / LockRows epqParam, F1-epq).
The builtin rule lists (which calls, patterns, divisors, privileges, aggregates, types and SQLValueFunctions) are read from
effects_facts.tsv, which decompose tests/effects_table.rs writes from the effects home's Rust answers; the walk and the
argument evaluators stay written here, and --check-table replays the table's probe rows through them.
Usage: graphprops_facts.py --check [<golden id> ...]   exit 1 naming every disagreeing line (all goldens by default)
       graphprops_facts.py --check-table               exit 1 naming every probe row an evaluator answers otherwise
       graphprops_facts.py <golden id>                 print the derived facts of one fixture
"""
import collections
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from graphprops import CATDIR, GOLDEN, dat, plan_path  # noqa: E402

INNER_VAR, OUTER_VAR = -1, -2
IMM, STA, VOL = 0, 1, 2
# C1 (one effects answer): the effects home's builtin rules, as crates/gravity/decompose/tests/effects_table.rs writes them
# from the Rust answers (REGEN=changed; ct fails when the file is not the Rust answers). Rows: rule, key, value.
EFFECTS_TSV = os.path.join(HERE, "effects_facts.tsv")


def effects_table():
    t = collections.defaultdict(dict)
    with open(EFFECTS_TSV) as f:
        for line in f:
            if line.startswith("#") or not line.strip():
                continue
            rule, key, value = line.rstrip("\n").split("\t")
            t[rule][key] = value
    return t


_T = effects_table()
FIRST_NORMAL_OID = 16384
# types_core::quiet::call: builtins raise-free beyond proleakproof, each rule needing no argument (effects::quiet_call)
QUIET = {int(k) for k in _T["call"]}
# effects::quiet_pattern (DC-14; rerun.rs F5 reads it for a FuncExpr / OpExpr since mergejoin's D1018 tail): a builtin
# pattern match (procfacts::PATTERNS: 1 LIKE, 2 ILIKE, 3 `~`, 4 `~*`, and their negations) of a non-NULL varlena constant
# pattern under a named deterministic collation (typefacts::KeyColl::of, the fixtures' facts: 0 is none, a builtin oid
# is deterministic, a user one is not): a LIKE / ILIKE pattern whose trailing backslashes pair up; a regex only when it
# compiles, and the fixtures' Facts::regex_compiles answers false
PATTERNS = {int(k): int(v) for k, v in _T["pattern"].items()}
# types_core::quiet::COLLATION_FREE (effects::collation_set): builtins that read no collation
COLLATION_FREE = {int(k) for k in _T["collation_free"]}
# types_core::quiet::const_arg / text_arg_quiet (effects::quiet_args): the constant's place, by argument count
TEXT_ARG = {int(k.split()[0]): (int(k.split()[1]), int(v)) for k, v in _T["const_arg"].items()}
# types_core::quiet::divisor_quiet (effects::quiet_args): int_mod by a non-null by-value constant other than 0 (D567),
# int_div by one other than 0 and -1 (div_by_const); the constant's low constlen bytes, sign-extended (int_word)
INT_MOD = {int(k) for k in _T["int_mod"]}
INT_DIV = {int(k) for k in _T["int_div"]}
# types_core::quiet::PRIVILEGES / privilege_quiet (effects::quiet_args): (the privilege names, a role's ADMIN OPTION, the
# argument count, the privilege text the last argument)
PRIVILEGES = {int(k): (v.split(" ", 2)[2].split(","), v.split(" ", 2)[1] == "1", int(v.split(" ", 2)[0])) for k, v in _T["privilege"].items()}
# types_core::quiet::EPOCH_NS_* / epoch_clock_value / start_time (D3882; effects::epoch_ns_class): the shape's three calls,
# its clocks and the bound on k
EPOCH_NS = (int(_T["epoch_ns"]["cast"]), int(_T["epoch_ns"]["mul"]), int(_T["epoch_ns"]["extract"]))
EPOCH_MAX_K = int(_T["epoch_ns"]["max_k"])
EPOCH_CLOCK = {int(k) for k, v in _T["epoch_clock"].items() if v == "1"}
START_TIME = {int(k) for k in _T["start_time"]}
# types_core::quiet::svf_raise_free: the SQLValueFunctionOps that raise on no input
SVF_RAISE_FREE = {int(k) for k, v in _T["svf_raise_free"].items() if v == "1"}
# effects::agg_oid_quiet (decompose effects/read.rs over ops/agg/trans/table.rs's classes): plain aggregates whose
# transitions and final raise on no input, and those that do only over a bounded numeric argument (typmod)
QUIET_AGGS = {int(k) for k, v in _T["agg"].items() if v == "plain"}
TYPMOD_AGGS = {int(k) for k, v in _T["agg"].items() if v == "typmod"}
# types_core::quiet::text_extremum_quiet: max / min(text) and the collations their comparison never fails under
TEXT_EXTREMUM = collections.defaultdict(set)
for _k in _T["text_extremum"]:
    TEXT_EXTREMUM[int(_k.split()[0])].add(int(_k.split()[1]))
# types_core::quiet::io_text_target, out_quiet (AGC-30) and xml_value_quiet (AGC-03)
IO_TEXT_TARGET = {int(k) for k in _T["io_text_target"]}
OUT_QUIET = {int(k) for k in _T["out_quiet"]}
XML_VALUE_QUIET = {int(k) for k in _T["xml_value"]}
POLY = {"anyarray", "anyelement", "anynonarray", "anyrange", "anymultirange", "record", "anycompatible", "anycompatiblearray",
        "anycompatiblenonarray", "anycompatiblerange", "anycompatiblemultirange", "internal", "any"}
ZEXT4 = {26, 28, 29, 24, 2202, 2203, 2204, 2205, 2206, 3734, 3769, 4089, 4096, 4191}


# ---------------------------------------------------------------------------------------------- the outfuncs reader
class Reader:
    """outfuncs' text form: `{TAG :field value ...}` nodes (a field named by its last dotted part), `(...)` lists (`(i ..)`,
    `(o ..)`, `(b ..)` number lists), `<>` for none, `[ .. ]` datum bytes after a length, backslash-escaped tokens."""

    def __init__(self, s):
        self.s, self.i = s, 0

    def ws(self):
        while self.i < len(self.s) and self.s[self.i] in " \n\t\r":
            self.i += 1

    def token(self):
        self.ws()
        j, out = self.i, []
        while j < len(self.s) and self.s[j] not in " \n\t\r(){}":
            if self.s[j] == "\\":
                j += 1
            out.append(self.s[j])
            j += 1
        self.i = j
        return "".join(out)

    def value(self):
        self.ws()
        c = self.s[self.i]
        if c == "{":
            return self.node()
        if c == "(":
            return self.list()
        if self.s.startswith("<>", self.i):
            self.i += 2
            return None
        return self.token()

    def list(self):
        self.i += 1
        self.ws()
        out = []
        if self.s[self.i] in "iobx" and self.s[self.i + 1] in " )":
            self.i += 1
            while True:
                self.ws()
                if self.s[self.i] == ")":
                    self.i += 1
                    return [int(x) for x in out]
                out.append(self.token())
        while True:
            self.ws()
            if self.s[self.i] == ")":
                self.i += 1
                return out
            out.append(self.value())

    def node(self):
        self.i += 1
        n = {"_": self.token()}
        while True:
            self.ws()
            if self.s[self.i] == "}":
                self.i += 1
                return n
            key = self.token()[1:].split(".")[-1]
            v = self.value()
            if key == "constvalue" and v is not None:
                self.ws()
                if self.i < len(self.s) and self.s[self.i] == "[":
                    j = self.s.index("]", self.i)
                    v = [int(x) for x in self.s[self.i + 1:j].split()]
                    self.i = j + 1
            n[key] = v


# The fixture's `-- func <oid> vol= strict= leakproof= ...` lines (decision 20: a function past pg_proc.dat as the capture read
# its pg_proc row), (vol, strict, leakproof) by oid; Facts::func answers them before the builtin table (decompose corpus.rs).
FUNCS = {}
# The fixture's `-- rel <oid> <name> am=<am> ...` lines: each relation's access method by oid (Facts::rel's `am`).
RELS = {}


def read_plan(path):
    with open(path, errors="replace") as f:
        lines = f.readlines()
    FUNCS.clear()
    RELS.clear()
    for ln in lines:
        w = ln.split()
        if w[:2] == ["--", "func"]:
            kv = dict(x.split("=", 1) for x in w[3:] if "=" in x)
            FUNCS[int(w[2])] = ({"i": IMM, "s": STA, "v": VOL}[kv["vol"]], kv["strict"] == "t", kv["leakproof"] == "t")
        elif w[:2] == ["--", "rel"] and len(w) > 2:
            RELS[num(w[2])] = dict(x.split("=", 1) for x in w[3:] if "=" in x).get("am")
    text = "".join(ln for ln in lines if not ln.startswith("--"))
    j = text.find("{PLANNEDSTMT")
    return Reader(text[j:]).node() if j >= 0 else None


def num(v, d=0):
    try:
        return int(v)
    except (TypeError, ValueError):
        return d


# ---------------------------------------------------------------------------------------------- the catalog
class Catalog:
    def __init__(self):
        names = {}
        self.procs = {}
        for e in dat("pg_proc.dat"):
            if "oid" not in e:
                continue
            o = int(e["oid"])
            names.setdefault(e["proname"], o)
            vol = {"i": IMM, "s": STA, "v": VOL}[e.get("provolatile", "i")]
            self.procs[o] = (vol, e.get("proisstrict", "t") == "t", e.get("proleakproof", "f") == "t")
        fam = {"%s/%s" % (f["opfmethod"], f["opfname"]): int(f["oid"]) for f in dat("pg_opfamily.dat")}
        sig, opers = {}, {}
        for e in dat("pg_operator.dat"):
            if "oid" not in e:
                continue
            o = int(e["oid"])
            sig["%s(%s,%s)" % (e["oprname"], e.get("oprleft", "0"), e.get("oprright", "0"))] = o
            poly = e.get("oprleft") in POLY or e.get("oprright") in POLY
            opers[o] = [names.get(e.get("oprcode", "-"), 0), e.get("oprcanhash", "f") == "t", 0, poly]
        for a in dat("pg_amop.dat"):
            if a.get("amopmethod") == "hash" and a.get("amopstrategy") == "1":
                op, f = opers[sig[a["amopopr"]]], fam[a["amopfamily"]]
                op[2] = f if op[2] == 0 else min(op[2], f)
        self.opers = {o: tuple(v) for o, v in opers.items()}
        self.types = {}
        for t in dat("pg_type.dat"):
            if "oid" not in t:
                continue
            ln = {"NAMEDATALEN": 64, "SIZEOF_POINTER": 8}.get(t.get("typlen"), None)
            ln = ln if ln is not None else num(t.get("typlen"), -1)
            self.types[int(t["oid"])] = (names[t["typinput"]], names[t["typoutput"]], ln, t.get("typbyval", "f") != "f")
            if "array_type_oid" in t:
                self.types[int(t["array_type_oid"])] = (names["array_in"], names["array_out"], -1, False)


_CAT = None


def catalog():
    global _CAT
    if _CAT is None:
        _CAT = Catalog()
    return _CAT


# ---------------------------------------------------------------------------------------------- expressions
TYPE_FIELD = {"VAR": "vartype", "CONST": "consttype", "PARAM": "paramtype", "OPEXPR": "opresulttype",
              "FUNCEXPR": "funcresulttype", "AGGREF": "aggtype", "WINDOWFUNC": "wintype", "RELABELTYPE": "resulttype",
              "COERCEVIAIO": "resulttype", "CASEEXPR": "casetype", "COALESCEEXPR": "coalescetype", "MINMAXEXPR": "minmaxtype",
              "ARRAYEXPR": "array_typeid", "SUBPLAN": "firstColType", "NULLIFEXPR": "opresulttype",
              "SUBSCRIPTINGREF": "refrestype", "FIELDSTORE": "resulttype", "COERCETODOMAIN": "resulttype",
              "NEXTVALUEEXPR": "typeId", "MERGESUPPORTFUNC": "msftype", "ROWEXPR": "row_typeid"}
COLL_FIELD = {"VAR": "varcollid", "CONST": "constcollid", "PARAM": "paramcollid", "OPEXPR": "opcollid", "FUNCEXPR": "funccollid",
              "AGGREF": "aggcollid", "WINDOWFUNC": "wincollid", "RELABELTYPE": "resultcollid", "COERCEVIAIO": "resultcollid",
              "CASEEXPR": "casecollid", "COALESCEEXPR": "coalescecollid", "MINMAXEXPR": "minmaxcollid",
              "ARRAYEXPR": "array_collid", "SUBPLAN": "firstColCollation", "NULLIFEXPR": "opcollid", "COLLATEEXPR": "collOid",
              "COERCETODOMAIN": "resultcollid", "COERCETODOMAINVALUE": "collation", "CASETESTEXPR": "collation",
              "ARRAYCOERCEEXPR": "resultcollid", "FIELDSELECT": "resultcollid", "SUBSCRIPTINGREF": "refcollid",
              "MERGESUPPORTFUNC": "msfcollid"}


def type_of(n):
    if not isinstance(n, dict):
        return 0
    t = n["_"]
    if t == "TARGETENTRY":
        return type_of(n.get("expr"))
    if t == "RETURNINGEXPR":   # exprType: its retexpr's type
        return type_of(n.get("retexpr"))
    if t in ("DISTINCTEXPR", "SCALARARRAYOPEXPR", "BOOLEXPR", "NULLTEST", "BOOLEANTEST", "ROWCOMPAREEXPR"):
        return 16
    if t == "GROUPINGFUNC":   # exprType: INT4OID (D1260: a SubPlan argument binds it as the Agg's folded int4 constant)
        return 23
    return num(n.get(TYPE_FIELD.get(t, "")), 0)


def collation_of(n):
    if not isinstance(n, dict):
        return 0
    t = n["_"]
    if t in ("TARGETENTRY", "RETURNINGEXPR"):
        return collation_of(n.get("expr" if t == "TARGETENTRY" else "retexpr"))
    if t == "SQLVALUEFUNCTION":
        return 950 if num(n.get("type")) == 19 else 0
    return num(n.get(COLL_FIELD.get(t, "")), 0)


def typmod_of(n):
    f = {"VAR": "vartypmod", "CONST": "consttypmod", "PARAM": "paramtypmod", "RELABELTYPE": "resulttypmod"}.get(n["_"])
    return num(n.get(f), -1) if f else -1


def through_relabel(n):
    if not isinstance(n, dict) or n["_"] != "RELABELTYPE":
        return None
    frm, to = type_of(n.get("arg")), num(n.get("resulttype"))
    same = (frm in ZEXT4) == (to in ZEXT4) if frm else (to not in ZEXT4 and to != 23)
    return n.get("arg") if same else None


LEAVES = {"VAR", "CONST", "PARAM", "CASETESTEXPR", "COERCETODOMAINVALUE", "SQLVALUEFUNCTION", "MERGESUPPORTFUNC"}
KIDS = {"TARGETENTRY": ["expr"], "OPEXPR": ["args"], "DISTINCTEXPR": ["args"], "NULLIFEXPR": ["args"], "FUNCEXPR": ["args"],
        "BOOLEXPR": ["args"], "SCALARARRAYOPEXPR": ["args"], "COALESCEEXPR": ["args"], "MINMAXEXPR": ["args"],
        "ARRAYEXPR": ["elements"], "ROWEXPR": ["args"], "GROUPINGFUNC": ["args"],
        "AGGREF": ["aggdirectargs", "args", "aggorder", "aggdistinct", "aggfilter"], "WINDOWFUNC": ["args", "aggfilter"],
        "ROWCOMPAREEXPR": ["largs", "rargs"], "RELABELTYPE": ["arg"], "COERCEVIAIO": ["arg"], "COLLATEEXPR": ["arg"],
        "COERCETODOMAIN": ["arg"], "CONVERTROWTYPEEXPR": ["arg"], "FIELDSELECT": ["arg"], "FIELDSTORE": ["arg", "newvals"],
        "ARRAYCOERCEEXPR": ["arg", "elemexpr"], "NULLTEST": ["arg"], "BOOLEANTEST": ["arg"],
        "CASEEXPR": ["arg", "args", "defresult"], "CASEWHEN": ["expr", "result"], "SUBPLAN": ["testexpr", "args"],
        "ALTERNATIVESUBPLAN": ["subplans"], "SUBSCRIPTINGREF": ["refexpr", "refupperindexpr", "reflowerindexpr", "refassgnexpr"],
        "JSONCONSTRUCTOREXPR": ["args", "func", "coercion"],
        "JSONEXPR": ["formatted_expr", "path_spec", "passing_values", "on_empty", "on_error"], "JSONBEHAVIOR": ["expr"],
        "JSONISPREDICATE": ["expr"], "XMLEXPR": ["named_args", "args"],
        # rerun.rs F5 (writesubplan D1112): a RETURNING view column's OLD / NEW image wrapper reads as its expression
        "RETURNINGEXPR": ["retexpr"]}
# agg_quiet: effects::agg_quiet over the table's QUIET_AGGS / TYPMOD_AGGS / TEXT_EXTREMUM; F5 (rerun.rs) keeps raise-free
# over one (D1108)


def agg_quiet(n):
    if n.get("aggkind") != "n" or num(n.get("aggsplit")) != 0:
        return False
    o = num(n.get("aggfnoid"))
    if o in QUIET_AGGS:
        return True
    if o in TEXT_EXTREMUM:  # types_core::quiet::text_extremum_quiet; 0, an indeterminate collation, raises 42P22
        return num(n.get("inputcollid")) in TEXT_EXTREMUM[o]
    first = (n.get("args") or [None])[0]
    var = first.get("expr") if isinstance(first, dict) else None
    return o in TYPMOD_AGGS and isinstance(var, dict) and var.get("_") == "VAR" and num(var.get("vartypmod"), -1) >= 0


RAISES = {"ARRAYCOERCEEXPR", "MINMAXEXPR", "SUBSCRIPTINGREF", "XMLEXPR", "JSONEXPR", "JSONCONSTRUCTOREXPR",
          "JSONISPREDICATE", "JSONVALUEEXPR", "JSONBEHAVIOR"}


def xml_quiet(n):
    vals = [x for f in ("named_args", "args") for x in (n.get(f) or []) if isinstance(x, dict)]
    return num(n.get("op"), -1) in (0, 1, 2) and all(type_of(x) in XML_VALUE_QUIET for x in vals)


def json_is_quiet(n):
    t = type_of(n.get("expr"))
    return t == 3802 or t == 114 and n.get("unique_keys") != "true"


class Walk:
    def __init__(self, stmt):
        self.stmt, self.cat = stmt, catalog()
        self.vol, self.raise_free, self.stable = IMM, True, True
        self.wt, self.seen = set(), set()

    def unknown(self):
        self.vol, self.raise_free, self.stable = VOL, False, False

    def heap(self, scanrelid):
        """Range-table entry `scanrelid` is a heap relation by the fixture's `-- rel` lines."""
        rt = self.stmt.get("rtable") or []
        e = rt[scanrelid - 1] if 0 < scanrelid <= len(rt) else None
        return isinstance(e, dict) and RELS.get(num(e.get("relid"))) == "heap"

    def func(self, oid, args=None, coll=None):
        p = FUNCS.get(oid) or self.cat.procs.get(oid)
        if p is None:
            return self.unknown()
        self.vol = max(self.vol, p[0])
        self.raise_free &= p[1] and (p[2] or oid in QUIET or divisor_quiet(oid, args) or privilege_quiet(oid, args) or text_arg_quiet(oid, args)
                                     or coll is not None and pattern_quiet(oid, args, coll))
        # effects::collation_set (found.rs call_raise_free): over arguments of conflicting implicit collations (input
        # collation 0) a collation-aware builtin raises 42P22; one that reads no collation (types_core::quiet::
        # COLLATION_FREE: the pattern comparisons, ||) needs none
        if coll is not None and coll == 0 and oid not in COLLATION_FREE and any(collation_of(a) for a in (args or []) if isinstance(a, dict)):
            self.raise_free = False

    def oper(self, opno, funcid=0, args=None, coll=None):
        f = funcid or self.cat.opers.get(opno, (0,))[0]
        self.func(f, args, coll) if f else self.unknown()

    def exprs(self, v):
        if isinstance(v, list):
            for x in v:
                self.exprs(x)
        elif isinstance(v, dict):
            self.expr(v)

    def expr(self, n):
        t = n["_"]
        if epoch_ns_shape(n):
            self.vol = max(self.vol, STA)
            return
        if t == "FUNCEXPR":
            self.func(num(n.get("funcid")), n.get("args"), num(n.get("inputcollid")))
        elif t == "OPEXPR":
            self.oper(num(n.get("opno")), num(n.get("opfuncid")), n.get("args"), num(n.get("inputcollid")))
        elif t in ("DISTINCTEXPR", "NULLIFEXPR"):
            self.oper(num(n.get("opno")), num(n.get("opfuncid")), n.get("args"), num(n.get("inputcollid")))
        elif t == "SCALARARRAYOPEXPR":
            self.oper(num(n.get("opno")), num(n.get("opfuncid")))
            for f in (num(n.get("hashfuncid")), num(n.get("negfuncid"))):
                if f:
                    self.func(f)
        elif t == "ROWCOMPAREEXPR":
            for op in n.get("opnos") or []:
                self.oper(op)
            # effects::row_compare_quiet: each column's family initdb's, its collation set
            fams, colls = n.get("opfamilies") or [], n.get("inputcollids") or []
            pairs = list(zip(n.get("largs") or [], n.get("rargs") or []))
            if len(fams) != len(pairs) or len(colls) != len(pairs) or not pairs or \
                    any(not f or f >= FIRST_NORMAL_OID for f in fams) or \
                    any(c == 0 and (collation_of(l) or collation_of(r)) for c, (l, r) in zip(colls, pairs)):
                self.raise_free = False
        elif t == "AGGREF":
            rf = self.raise_free
            self.func(num(n.get("aggfnoid")))
            if agg_quiet(n):
                self.raise_free = rf
        elif t == "WINDOWFUNC":
            self.func(num(n.get("winfnoid")))
        elif t == "COERCEVIAIO":
            out = self.cat.types.get(type_of(n.get("arg")))
            inp = self.cat.types.get(num(n.get("resulttype")))
            rf = self.raise_free
            self.func(out[1]) if out else self.unknown()
            self.func(inp[0]) if inp else self.unknown()
            # types_core::quiet::io_quiet (AGC-30): to a text-like type from a listed source neither function raises
            if type_of(n.get("arg")) in OUT_QUIET and num(n.get("resulttype")) in IO_TEXT_TARGET:
                self.raise_free = rf
            else:
                self.raise_free = False
        elif t == "COERCETODOMAIN":
            # decompose effects/found.rs Found::DOMAIN_CHECK (fail closed: a fixture's facts carry no domain's constraints,
            # Facts::domain_checks; D3796: a server's do): PostgreSQL runs every CHECK of the domain chain at each coercion
            # and a CHECK may call user or VOLATILE code.
            self.vol, self.raise_free = VOL, False
        elif t == "XMLEXPR" and xml_quiet(n):
            pass   # effects::xml_quiet (AGC-03): XMLCONCAT / XMLELEMENT / XMLFOREST over listed value types
        elif t == "JSONISPREDICATE" and json_is_quiet(n):
            pass   # effects::json_is_quiet (AGC-31): over jsonb, or json without UNIQUE KEYS
        elif t in RAISES or (t == "ARRAYEXPR" and n.get("multidims") == "true"):
            self.raise_free = False
        elif t == "SQLVALUEFUNCTION":
            self.vol = max(self.vol, STA)
            # D3920 (types_core::quiet::svf_raise_free, SVF_RAISE_FREE): the role ops raise 'invalid role OID' once another
            # session dropped the role; the rest raise on no input
            if num(n.get("op"), -1) not in SVF_RAISE_FREE:
                self.raise_free = False
        elif t == "SUBPLAN":
            self.subplan(n)
        elif t == "SORTGROUPCLAUSE":
            for o in (num(n.get("eqop")), num(n.get("sortop"))):
                if o:
                    self.oper(o)
            return
        elif t == "WINDOWFUNCRUNCONDITION":
            self.oper(num(n.get("opno")))
            self.exprs(n.get("arg"))
            return
        if t in LEAVES:
            return
        if t not in KIDS:
            return self.unknown()
        for k in KIDS[t]:
            self.exprs(n.get(k))

    def root(self, plan_id):
        subs = self.stmt.get("subplans") or []
        return subs[plan_id - 1] if 1 <= plan_id <= len(subs) and isinstance(subs[plan_id - 1], dict) else None

    def subplan(self, s):
        pid = num(s.get("plan_id"))
        r = self.root(pid)
        if s.get("subLinkType") in ("4", "3", "5", "EXPR_SUBLINK", "ROWCOMPARE_SUBLINK", "MULTIEXPR_SUBLINK") and not one_row(r):
            self.raise_free = False
        if pid in self.seen:
            return
        self.seen.add(pid)
        self.plan(r) if r else self.unknown()

    def ops(self, v):
        for o in v or []:
            self.oper(num(o))

    def bound(self, v):
        if v is not None and not (v["_"] == "CONST" and (v.get("constisnull") == "true" or const_i64(v) >= 0)):
            self.raise_free = False
        if v is not None:
            self.expr(v)

    def plan(self, n):
        t = n["_"]
        self.exprs(n.get("targetlist"))
        self.exprs(n.get("qual"))
        self.exprs(n.get("initPlan"))
        if t in ("SEQSCAN", "MATERIAL", "PROJECTSET", "BITMAPAND", "BITMAPOR", "SUBQUERYSCAN"):
            pass
        elif t == "RESULT":
            self.exprs(n.get("resconstantqual"))
        elif t == "INDEXSCAN":
            for k in ("indexqual", "indexqualorig", "indexorderby", "indexorderbyorig"):
                self.exprs(n.get(k))
        elif t == "INDEXONLYSCAN":
            for k in ("indexqual", "recheckqual", "indexorderby"):
                self.exprs(n.get(k))
        elif t == "BITMAPINDEXSCAN":
            self.exprs(n.get("indexqual"))
            self.exprs(n.get("indexqualorig"))
        elif t in ("BITMAPHEAPSCAN", "TIDSCAN", "TIDRANGESCAN"):
            self.exprs(n.get({"BITMAPHEAPSCAN": "bitmapqualorig", "TIDSCAN": "tidquals", "TIDRANGESCAN": "tidrangequals"}[t]))
        elif t == "SAMPLESCAN":
            ts = n.get("tablesample")
            ts = ts if isinstance(ts, dict) else {}
            # D1920 / D1644 (decompose rerun.rs): a heap BERNOULLI / SYSTEM sample samples the same rows at every run of its
            # plan node in an execution (the node's one seed, or its REPEATABLE value's); its clause's own raise is
            # effects::sample_class's
            if num(ts.get("tsmhandler")) not in (3313, 3314) or not self.heap(num(n.get("scanrelid"))):
                self.stable = False
            if not sample_quiet(ts):
                self.raise_free = False
            self.exprs(ts.get("args"))
            self.exprs(ts.get("repeatable"))
        elif t == "FUNCTIONSCAN":
            for f in n.get("functions") or []:
                self.exprs(f.get("funcexpr")) if isinstance(f, dict) and f["_"] == "RANGETBLFUNCTION" else self.unknown()
        elif t == "VALUESSCAN":
            self.exprs(n.get("values_lists"))
        elif t == "CTESCAN":
            pid = num(n.get("ctePlanId"))
            if pid not in self.seen:
                self.seen.add(pid)
                r = self.root(pid)
                self.plan(r) if r else self.unknown()
        elif t == "WORKTABLESCAN":
            if num(n.get("wtParam")) not in self.wt:
                self.stable = False
        elif t == "NAMEDTUPLESTORESCAN":
            self.stable = False
        elif t == "RECURSIVEUNION":
            self.wt.add(num(n.get("wtParam")))
            self.ops(n.get("dupOperators"))
        elif t == "NESTLOOP":
            self.exprs(n.get("joinqual"))
            for p in n.get("nestParams") or []:
                self.exprs(p.get("paramval")) if isinstance(p, dict) and p["_"] == "NESTLOOPPARAM" else self.unknown()
        elif t == "MERGEJOIN":
            self.exprs(n.get("joinqual"))
            self.exprs(n.get("mergeclauses"))
        elif t == "HASHJOIN":
            for k in ("joinqual", "hashclauses", "hashkeys"):
                self.exprs(n.get(k))
        elif t == "HASH":
            self.exprs(n.get("hashkeys"))
        elif t in ("SORT", "INCREMENTALSORT", "MERGEAPPEND"):
            self.ops(n.get("sortOperators"))
            if t == "MERGEAPPEND" and num(n.get("part_prune_index"), -1) >= 0:
                self.raise_free = False
        elif t in ("GROUP", "AGG"):
            self.ops(n.get("grpOperators"))
        elif t == "UNIQUE":
            self.ops(n.get("uniqOperators"))
        elif t == "SETOP":
            self.ops(n.get("cmpOperators"))
        elif t == "WINDOWAGG":
            self.ops(n.get("partOperators"))
            self.ops(n.get("ordOperators"))
            if n.get("startOffset") is not None or n.get("endOffset") is not None:
                self.raise_free = False
            self.exprs(n.get("startOffset"))
            self.exprs(n.get("endOffset"))
            self.exprs(n.get("runCondition"))
            for f in (num(n.get("startInRangeFunc")), num(n.get("endInRangeFunc"))):
                if f:
                    self.func(f)
        elif t == "LIMIT":
            cnt = n.get("limitCount")
            if n.get("limitOption") in ("1", "LIMIT_OPTION_WITH_TIES") and not (cnt and cnt["_"] == "CONST" and cnt.get("constisnull") != "true"):
                self.raise_free = False
            self.bound(cnt)
            self.bound(n.get("limitOffset"))
            self.ops(n.get("uniqOperators"))
        elif t == "MEMOIZE":
            self.exprs(n.get("param_exprs"))
            self.ops(n.get("hashOperators"))
        elif t == "APPEND":
            self.ops(n.get("gravity_sort_operators"))
            if num(n.get("part_prune_index"), -1) >= 0:
                self.raise_free = False
        elif t == "LOCKROWS":
            self.stable, self.raise_free = False, False
        else:
            self.unknown()
        for k in plan_kids(n):
            self.plan(k)


def divisor_quiet(oid, args):
    """An integer `%` / `/` call whose divisor (its second argument) is a constant the rule passes (INT_MOD / INT_DIV)."""
    if oid not in INT_MOD | INT_DIV or not isinstance(args, list) or len(args) != 2:
        return False
    c = args[1]
    if not isinstance(c, dict) or c.get("_") != "CONST" or c.get("constisnull") == "true" or c.get("constbyval") != "true":
        return False
    b, n = c.get("constvalue"), num(c.get("constlen"))
    if not isinstance(b, list) or n not in (2, 4, 8):
        return False
    d = int.from_bytes(bytes(b[:n]), "little", signed=True)
    return d != 0 and (oid in INT_MOD or d != -1)


def sample_quiet(ts):
    """effects::sample_class: a BERNOULLI / SYSTEM clause of one non-null float4 constant percentage in 0 .. 100 and no
    REPEATABLE value or a non-null constant one raises nothing at its scan's start (tablesample_init, BeginSampleScan)."""
    import struct
    if num(ts.get("tsmhandler")) not in (3313, 3314):
        return False
    args = ts.get("args")
    if not isinstance(args, list) or len(args) != 1:
        return False
    c = args[0]
    if not isinstance(c, dict) or c.get("_") != "CONST" or c.get("constisnull") == "true" or c.get("constbyval") != "true" \
            or num(c.get("consttype")) != 700 or not isinstance(c.get("constvalue"), list):
        return False
    p = struct.unpack("<f", bytes(c["constvalue"][:4]))[0]
    r = ts.get("repeatable")
    return 0.0 <= p <= 100.0 and (not isinstance(r, dict) or r.get("_") == "CONST" and r.get("constisnull") != "true")


def varlena_payload(c):
    """A non-NULL by-reference varlena constant's payload bytes (expr::const_bytes_keyed); None for any other constant."""
    if not isinstance(c, dict) or c.get("_") != "CONST" or c.get("constisnull") == "true" or c.get("constbyval") == "true" \
            or num(c.get("constlen")) != -1:
        return None
    b = c.get("constvalue")
    if not isinstance(b, list) or not b:
        return None
    if b[0] & 1:
        return None if b[0] == 1 else bytes(b[1:(b[0] >> 1) & 0x7F])
    if b[0] & 3 == 0 and len(b) >= 4:
        return bytes(b[4:int.from_bytes(bytes(b[:4]), "little") >> 2])
    return None


def pattern_quiet(oid, args, coll):
    """effects::quiet_pattern over the fixtures' facts: PATTERNS' call of a good constant pattern (see PATTERNS)."""
    form = PATTERNS.get(oid)
    if form is None or not isinstance(args, list) or len(args) != 2 or not coll or coll >= FIRST_NORMAL_OID:
        return False
    pat = varlena_payload(args[1])
    if pat is None or form > 2:
        return False
    return (len(pat) - len(pat.rstrip(b"\\"))) % 2 == 0


def text_arg_quiet(oid, args):
    """A substr / left / right call whose length or n (TEXT_ARG's place) is a by-value constant the rule passes."""
    if oid not in TEXT_ARG or not isinstance(args, list) or len(args) != TEXT_ARG[oid][0]:
        return False
    c = args[TEXT_ARG[oid][1]]
    if not isinstance(c, dict) or c.get("_") != "CONST" or c.get("constisnull") == "true" or c.get("constbyval") != "true":
        return False
    b, n = c.get("constvalue"), num(c.get("constlen"))
    if not isinstance(b, list) or n not in (2, 4, 8):
        return False
    d = int.from_bytes(bytes(b[:n]), "little", signed=True)
    return d >= 0 if oid in (877, 936) else d != -2 ** 31


def privilege_quiet(oid, args):
    """A PRIVILEGES call whose privilege text (its last argument, a constant) every chunk of names a privilege."""
    if oid not in PRIVILEGES or not isinstance(args, list) or len(args) != PRIVILEGES[oid][2]:
        return False
    c = args[-1]
    if not isinstance(c, dict) or c.get("_") != "CONST" or c.get("constisnull") == "true" or num(c.get("constlen")) != -1:
        return False
    b = c.get("constvalue")
    if not isinstance(b, list) or not b:
        return False
    if b[0] & 1:
        text = bytes(b[1:(b[0] >> 1) & 0x7F])
    elif b[0] & 3 == 0 and len(b) >= 4:
        text = bytes(b[4:int.from_bytes(bytes(b[:4]), "little") >> 2])
    else:
        return False
    names, role, _ = PRIVILEGES[oid]
    opts = ["", " WITH GRANT OPTION"] + ([" WITH ADMIN OPTION"] if role else [])
    ok = {(n + o).upper() for n in names for o in opts}
    return all(ch.strip(b" \t\n\x0b\x0c\r").decode("latin-1").upper() in ok and ch.strip(b" \t\n\x0b\x0c\r").isascii()
               for ch in text.split(b","))


def call_of(n):
    """A FuncExpr's or OpExpr's function and arguments; None for any other node."""
    if isinstance(n, dict) and n.get("_") == "FUNCEXPR":
        return num(n.get("funcid")), n.get("args")
    if isinstance(n, dict) and n.get("_") == "OPEXPR":
        return num(n.get("opfuncid")), n.get("args")
    return None


def epoch_ns_k_quiet(b):
    """types_core::quiet::epoch_ns_k_quiet: a numeric image (little-endian) that is finite with |v| <= 1e9."""
    if len(b) < 2:
        return False
    h = int.from_bytes(b[:2], "little")
    if h & 0xC000 == 0x8000:
        w, at = ((h & 0x3F) - 64 if h & 0x40 else h & 0x3F), 2
    elif h & 0xC000 in (0x0000, 0x4000) and len(b) >= 4:
        w, at = int.from_bytes(b[2:4], "little", signed=True), 4
    else:
        return False
    if (len(b) - at) % 2:
        return False
    whole, frac = 0, False
    for i in range((len(b) - at) // 2):
        d, e = int.from_bytes(b[at + 2 * i:at + 2 * i + 2], "little"), w - i
        if d >= 10000:
            return False
        if d == 0:
            continue
        if e >= 3:
            return False
        if e >= 0:
            whole += d * 10000 ** e
        else:
            frac = True
    return whole < EPOCH_MAX_K or whole == EPOCH_MAX_K and not frac


def epoch_ns_shape(n):
    """effects::epoch_ns_class's shape (EPOCH_NS): (extract(epoch FROM <clock>) * k)::int8, read whole."""
    c = call_of(n)
    if not c or c[0] != EPOCH_NS[0] or not isinstance(c[1], list) or len(c[1]) != 1:
        return False
    m = call_of(c[1][0])
    if not m or m[0] != EPOCH_NS[1] or not isinstance(m[1], list) or len(m[1]) != 2:
        return False
    e, k = m[1]
    x = call_of(e)
    if not x or x[0] != EPOCH_NS[2] or not isinstance(x[1], list) or len(x[1]) != 2:
        return False
    unit, clock = x[1]
    u = varlena_payload(unit)
    if u is None or u.lower() != b"epoch" or not isinstance(clock, dict):
        return False
    if not (clock.get("_") == "FUNCEXPR" and num(clock.get("funcid")) in START_TIME and not clock.get("args")
            or clock.get("_") == "SQLVALUEFUNCTION" and num(clock.get("op"), -1) in EPOCH_CLOCK):
        return False
    kb = varlena_payload(k)  # numeric_mul's second argument: its image is a numeric's, as in epoch_ns_class
    return kb is not None and epoch_ns_k_quiet(kb)


def const_i64(c):
    b = c.get("constvalue")
    if c.get("constbyval") != "true" or num(c.get("constlen")) != 8 or not isinstance(b, list):
        return -1  # not a bound decompose reads as a constant
    return int.from_bytes(bytes(b[:8]), "little", signed=True)


def plan_kids(n):
    out = [n[k] for k in ("lefttree", "righttree") if isinstance(n.get(k), dict)]
    for k in {"APPEND": "appendplans", "MERGEAPPEND": "mergeplans", "BITMAPAND": "bitmapplans", "BITMAPOR": "bitmapplans",
              "AGG": "chain"}.get(n["_"], "-").split():
        out += [x for x in n.get(k) or [] if isinstance(x, dict)]
    if n["_"] == "SUBQUERYSCAN" and isinstance(n.get("subplan"), dict):
        out.append(n["subplan"])
    return out


# AGB-28: decompose catalog/rows.rs `max_rows` as a source without the catalog reads it (Facts::unique_bound false: a scan
# proves no key), so only the plan's own bounds count: a child-less Result, a plain Agg, a constant LIMIT (not WITH TIES),
# a VALUES list, the nodes that pass rows through, appends (the sum) and joins over bounded children.
PASS_ROWS = ("SORT", "INCREMENTALSORT", "MATERIAL", "MEMOIZE", "UNIQUE", "GROUP", "LOCKROWS", "WINDOWAGG", "HASH")


def max_rows(r):
    if not isinstance(r, dict):
        return None
    t = r["_"]
    if t == "RESULT":
        return max_rows(r["lefttree"]) if isinstance(r.get("lefttree"), dict) else 1
    if t == "AGG":
        if r.get("groupingSets"):
            return None
        return 1 if num(r.get("aggstrategy")) == 0 else max_rows(r.get("lefttree"))
    if t == "LIMIT":
        below, c = max_rows(r.get("lefttree")), r.get("limitCount")
        if c and c["_"] == "CONST" and c.get("constisnull") != "true" and const_i64(c) >= 0 and num(r.get("limitOption")) != 1:
            return const_i64(c) if below is None else min(below, const_i64(c))
        return below
    if t in PASS_ROWS:
        return max_rows(r.get("lefttree"))
    if t == "SUBQUERYSCAN":
        return max_rows(r.get("subplan"))
    if t in ("APPEND", "MERGEAPPEND"):
        total = 0
        for x in r.get("appendplans" if t == "APPEND" else "mergeplans") or []:
            m = max_rows(x)
            if m is None:
                return None
            total += m
        return total
    if t in ("NESTLOOP", "HASHJOIN", "MERGEJOIN"):
        o, i, jt = max_rows(r.get("lefttree")), max_rows(r.get("righttree")), num(r.get("jointype"))
        if jt in (4, 5):
            return o
        if jt in (6, 7):
            return i
        if jt in (0, 1) and r.get("inner_unique") == "true":
            return o
        if o is None or i is None:
            return None
        return {0: o * i, 1: o * max(i, 1), 3: i * max(o, 1)}.get(jt)
    if t == "VALUESSCAN":
        return len(r.get("values_lists") or [])
    return None


def one_row(r):
    m = max_rows(r)
    return m is not None and m <= 1


def each_node(n, f):
    f(n)
    for k in plan_kids(n):
        each_node(k, f)


def fixed_params(stmt):
    inits = []
    roots = [stmt.get("planTree")] + list(stmt.get("subplans") or [])
    for r in roots:
        if isinstance(r, dict):
            each_node(r, lambda n: inits.extend(s for s in n.get("initPlan") or [] if isinstance(s, dict)))
    w, fixed = Walk(stmt), set()
    while True:
        grew = False
        for s in inits:
            sets = set(s.get("setParam") or [])
            if sets <= fixed:
                continue
            r = w.root(num(s.get("plan_id")))
            if r and set(r.get("extParam") or []) <= fixed:
                fixed |= sets
                grew = True
        if not grew:
            return fixed


def facts_of(stmt, fn):
    w = Walk(stmt)
    fn(w)
    return w


def f5(w):
    return (" vol=%s" % "isv"[w.vol] if w.vol else "") + (" raise-free" if w.raise_free else "")


def epq_params(stmt):
    """F1-epq (rerun.rs `invariant`): each ModifyTable / LockRows epqParam, which only the EvalPlanQual tree sets."""
    out = set()
    for r in [stmt.get("planTree")] + list(stmt.get("subplans") or []):
        if isinstance(r, dict):
            each_node(r, lambda n: out.add(num(n.get("epqParam"), -1)) if n.get("_") in ("MODIFYTABLE", "LOCKROWS") else None)
    return out


def invariant(stmt, root, w):
    return w.vol != VOL and w.stable and set(root.get("allParam") or []) <= fixed_params(stmt) | epq_params(stmt)


def eq_conjunct(stmt, qual):
    cat = catalog()
    for i, c in enumerate(qual or []):
        e = eq_of(stmt, c, i, cat)
        if e:
            return e
        w = facts_of(stmt, lambda w: w.expr(c))
        if w.vol == VOL or not w.raise_free:
            return None
    return None


def eq_of(stmt, c, pos, cat):
    if c["_"] != "OPEXPR" or len(c.get("args") or []) != 2:
        return None
    op = cat.opers.get(num(c.get("opno")))
    if not op or not op[1] or not op[2] or op[3]:
        return None

    def inner_col(n):
        v = through_relabel(n) or n
        if v["_"] == "VAR" and num(v.get("varno")) == INNER_VAR and num(v.get("varlevelsup")) == 0 and num(v.get("varattno")) > 0:
            return num(v.get("varattno"))
        return None
    l, r = c["args"]
    li, ri = inner_col(l), inner_col(r)
    if (li is None) == (ri is None):
        return None
    outer, inner, left = (l, r, True) if ri is not None else (r, l, False)
    found = []

    def reads_inner(n):
        if isinstance(n, dict):
            if n["_"] == "VAR" and num(n.get("varno")) == INNER_VAR:
                found.append(1)
            for v in n.values():
                reads_inner(v)
        elif isinstance(n, list):
            for v in n:
                reads_inner(v)
    reads_inner(outer)
    w = facts_of(stmt, lambda w: w.expr(outer))
    if found or w.vol == VOL:
        return None
    coll = num(c.get("inputcollid"))
    return " eq=#%d:op%d:fam%d:%d/%d%s:%s%s" % (pos, num(c.get("opno")), op[2], type_of(outer), type_of(inner),
                                                 "@%d" % coll if coll else "", "L" if left else "R", "" if w.raise_free else ":raises")


# ---------------------------------------------------------------------------------------------- the statement's facts
def all_nodes(v, out):
    if isinstance(v, dict):
        out.append(v)
        for x in v.values():
            all_nodes(x, out)
    elif isinstance(v, list):
        for x in v:
            all_nodes(x, out)
    return out


def hoist_values(stmt, inner):
    """D2013 slice 2 (decompose facts/hoist.rs): the values holding a SubPlan in nest inner `inner`'s index / recheck quals
    (the clause side holding it) and its unions' run-time pruning steps, as (synthetic param id, value): the hoist makes one
    a param of the nest, $exec(len(paramExecTypes) + its first SubPlan's plan_id - 1). Which candidates hoist is the effects
    home's call (facts/hoist.rs `plan`, effects `hoist_ok`), read from the golden by `check`."""
    base = len(stmt.get("paramExecTypes") or [])
    infos = stmt.get("partPruneInfos") or []
    out = []

    def add(v):
        first = next((num(x.get("plan_id")) for x in all_nodes(v, []) if x["_"] == "SUBPLAN"), None)
        if first is not None:
            out.append((base + first - 1, v))
    for x in all_nodes(inner, []):
        t = x.get("_")
        if t in ("INDEXSCAN", "INDEXONLYSCAN", "BITMAPINDEXSCAN"):
            for c in (x.get("indexqual") or []) + (x.get("recheckqual") or []) + (x.get("indexqualorig") or []):
                for a in (c.get("args") or []) if isinstance(c, dict) else []:
                    add(a)
        if t in ("APPEND", "MERGEAPPEND") and 0 <= num(x.get("part_prune_index"), -1) < len(infos):
            for r in all_nodes(infos[num(x.get("part_prune_index"))], []):
                if r.get("_") == "PARTITIONEDRELPRUNEINFO":
                    for st in all_nodes(r.get("exec_pruning_steps"), []):
                        if st.get("_") == "PARTITIONPRUNESTEPOP":
                            for e in st.get("exprs") or []:
                                add(e)
    return out


def derive(stmt):
    """{('nest', inner node id): tokens, ('subplan', plan id, nested): tokens, ('init', plan id): tokens}, the params and
    the hoist candidates (`hoist_values`: inner node id, param id, owner, value)."""
    nodes = all_nodes(stmt, [])
    want, params, hoists = {}, [], []
    subs = {}
    for n in nodes:
        if n["_"] == "SUBPLAN":
            subs.setdefault(num(n.get("plan_id")), n)
        if n["_"] == "NESTLOOP" and isinstance(n.get("righttree"), dict):
            inner = n["righttree"]
            w = facts_of(stmt, lambda w: w.plan(inner))
            tok = (" invariant" if invariant(stmt, inner, w) else "") + f5(w) + (eq_conjunct(stmt, n.get("joinqual")) or "")
            iid = num(inner.get("plan_node_id"))
            want[("nest", iid)] = tok
            for p in n.get("nestParams") or []:
                v = p.get("paramval")
                params.append((iid, num(p.get("paramno")), "n%d" % num(n.get("plan_node_id")), v))
            hoists += [(iid, pno, "n%d" % num(n.get("plan_node_id")), v) for pno, v in hoist_values(stmt, inner)]
    for pid, s in subs.items():
        w = facts_of(stmt, lambda w: (w.exprs(s.get("testexpr")), w.exprs(s.get("args")), w.subplan(s)))
        r = Walk(stmt).root(pid)
        inv = False
        if r is not None and s.get("useHashTable") != "true":
            inv = invariant(stmt, r, facts_of(stmt, lambda w: w.plan(r)))
        want[("subplan", pid)] = (" invariant" if inv else "") + f5(w)
        for pno, a in zip(s.get("parParam") or [], s.get("args") or []):
            params.append((None, pno, "subplan%d" % pid, a))
    return want, params, hoists


def storage_ok(rep, typid):
    t = catalog().types.get(typid)
    if rep == "?" or t is None:
        return True
    _, _, ln, byval = t
    if byval:
        return re.fullmatch(r"w\d", rep) is not None
    if ln == -1:
        return rep == "vl" or rep.startswith("code") or rep.startswith("pn")
    if ln == -2:
        return rep == "cs"
    return rep == "fx%d" % ln or rep.startswith("code")


NEST = re.compile(r"^ *nest inner=n(-?\d+) \w+(?: unique)?((?: \S+)*?) params=\(")
SUBPLAN = re.compile(r"^ *subplan (\d+) (\w+) \w+.*?((?: invariant)?(?: vol=[sv])?(?: raise-free)?)(?: test=.*)?$")
INIT = re.compile(r"^init_plan id=(\d+) .*?((?: vol=[sv])?(?: raise-free)?)$")
PARAM = re.compile(r"^param \$exec(\d+) owner=(\S+) type=(\d+)(?:/(-?\d+))?(?:@(\d+))? rep=([^:\s]+)")


def check(fid):
    """The disagreeing lines of golden `fid` (empty when it agrees or has no graph / plan)."""
    gp = os.path.join(GOLDEN, "regress-full", fid[len("regress-full-"):] + ".graph") if fid.startswith("regress-full-") \
        else os.path.join(GOLDEN, fid + ".graph")
    pp = plan_path(fid)
    if not os.path.exists(gp) or not os.path.exists(pp):
        return []
    text = open(gp, errors="replace").read()
    if not any(k in text for k in ("nest inner=", "subplan ", "init_plan ", "\nparam ")):
        return []
    stmt = read_plan(pp)
    if stmt is None:
        return []
    want, params, hoists = derive(stmt)
    bad, got_params = [], []
    for ln in text.split("\n"):
        m = NEST.match(ln)
        if m:
            w = want.get(("nest", int(m.group(1))))
            if w is None or m.group(2) != w:
                bad.append("%s: %r: the mirror says %r" % (fid, ln.strip()[:160], w))
            continue
        m = SUBPLAN.match(ln)
        if m and m.group(2) in ("nested", "build", "hashed"):
            w = want.get(("subplan", int(m.group(1))))
            if w is None or m.group(3) != w:
                bad.append("%s: %r: the mirror says %r" % (fid, ln.strip()[:160], w))
            continue
        m = INIT.match(ln)
        if m:
            s = Walk(stmt)
            init = [x for x in all_nodes(stmt, []) if x["_"] == "SUBPLAN" and num(x.get("plan_id")) == int(m.group(1))]
            if init:
                s.subplan(init[0])
            w = f5(s) if init else None
            if w is None or m.group(2) != w:
                bad.append("%s: %r: the mirror says %r" % (fid, ln.strip()[:160], w))
            continue
        m = PARAM.match(ln)
        if m:
            got_params.append((int(m.group(1)), m.group(2), int(m.group(3)), int(m.group(4) or -1), int(m.group(5) or 0), m.group(6)))
    # F6: the params of the owners the graph maps, once per op line (a grouping sets arm or an inherited target maps the
    # same plan node again: each copy binds its params), as a multiset
    lines = text.split("\n")
    nests = collections.Counter(int(m.group(1)) for m in map(NEST.match, lines) if m)
    subs = collections.Counter(int(m.group(1)) for m in map(SUBPLAN.match, lines) if m and m.group(2) == "nested")
    # D2013 slice 2: a nest's hoisted value (`hoist_values`) is one more param of that nest, its id and owner as the golden
    # lists them (one per id: equal values share it), its type, typmod and collation the value's
    listed = {(p[0], p[1]) for p in got_params}
    seen = set()
    for iid, pno, owner, v in hoists:
        if (pno, owner) in listed and (iid, pno) not in seen and all(q[1] != pno for q in params):
            seen.add((iid, pno))
            params.append((iid, pno, owner, v))
    expect = sorted((pno, owner, type_of(v), typmod_of(v), collation_of(v)) for iid, pno, owner, v in params
                    for _ in range(nests[iid] if iid is not None else subs[int(owner[7:])]))
    have = sorted(p[:5] for p in got_params)
    if expect != have:
        bad.append("%s: param lines %s, the mirror says %s" % (fid, have, expect))
    for p in got_params:
        if not storage_ok(p[5], p[2]):
            bad.append("%s: param $exec%d rep=%s is no storage of type %d" % (fid, p[0], p[5], p[2]))
    return bad


def golden_ids():
    out = []
    for d, pre in ((GOLDEN, ""), (os.path.join(GOLDEN, "regress-full"), "regress-full-")):
        out += [pre + f[:-6] for f in sorted(os.listdir(d)) if f.endswith(".graph") and re.fullmatch(r"[\w-]+\.graph", f)]
    return out


def check_table():
    """The table's probe rows (effects_table.rs PROBE_*): the Rust answers over fixed inputs, each replayed through the
    mirror's own evaluator above. A row answered otherwise is a mirror that no longer reads an argument as Rust does."""
    def const(v):
        if v is None:
            return {"_": "CONST", "constisnull": "true"}
        return {"_": "CONST", "constisnull": "false", "constbyval": "true", "constlen": "8",
                "constvalue": list(v.to_bytes(8, "little", signed=True))}

    def text(b):
        return {"_": "CONST", "constisnull": "false", "constbyval": "false", "constlen": "-1",
                "constvalue": [((len(b) + 1) << 1) | 1] + list(b)}

    def word(s):
        return None if s == "null" else int(s)

    def unhex(s):
        return b"" if s == "-" else bytes.fromhex(s)

    dummy = {"_": "VAR"}
    bad, n = [], 0
    for key, value in sorted(_T["probe.divisor"].items()):
        oid, d = key.split()
        n += 1
        if int(divisor_quiet(int(oid), [dummy, const(word(d))])) != int(value):
            bad.append("probe.divisor %s: Rust %s" % (key, value))
    for key, value in sorted(_T["probe.text_arg"].items()):
        oid, w = int(key.split()[0]), word(key.split()[1])
        nargs, place = TEXT_ARG[oid]
        args = [dummy] * nargs
        args[place] = const(w)
        n += 1
        if int(text_arg_quiet(oid, args)) != int(value):
            bad.append("probe.text_arg %s: Rust %s" % (key, value))
    for key, value in sorted(_T["probe.privilege"].items()):
        oid, h = int(key.split()[0]), key.split()[1]
        args = [dummy] * (PRIVILEGES[oid][2] - 1) + [text(unhex(h))]
        n += 1
        if int(privilege_quiet(oid, args)) != int(value):
            bad.append("probe.privilege %s (%r): Rust %s" % (key, unhex(h), value))
    for key, value in sorted(_T["probe.pattern"].items()):
        oid, coll, h = key.split()
        n += 1
        if int(pattern_quiet(int(oid), [dummy, text(unhex(h))], int(coll))) != int(value):
            bad.append("probe.pattern %s (%r): Rust %s" % (key, unhex(h), value))
    for key, value in sorted(_T["probe.epoch_k"].items()):
        n += 1
        if int(epoch_ns_k_quiet(unhex(key))) != int(value.split()[0]):
            bad.append("probe.epoch_k %s (%s): Rust %s" % (key, value.split(" ", 1)[1], value.split()[0]))
    return n, bad


def main(argv):
    if argv and argv[0] == "--check-table":
        n, bad = check_table()
        for b in bad:
            print(b, file=sys.stderr)
        print("graphprops_facts: %d probe rows, %d answered otherwise than the effects home" % (n, len(bad)))
        return 1 if bad else 0
    if argv and argv[0] == "--check":
        ids = argv[1:] or golden_ids()
        bad, n = [], 0
        for fid in ids:
            b = check(fid)
            n += 1
            bad += b
        for b in bad:
            print(b, file=sys.stderr)
        print("graphprops_facts: %d goldens, %d disagreeing lines" % (n, len(bad)))
        return 1 if bad else 0
    for fid in argv:
        stmt = read_plan(plan_path(fid))
        want, params, hoists = derive(stmt)
        for k, v in sorted(want.items(), key=str):
            print(fid, k, repr(v))
        for p in params:
            print(fid, "param", p[1], p[2], type_of(p[3]), typmod_of(p[3]), collation_of(p[3]))
        for p in hoists:
            print(fid, "hoist candidate", p[1], p[2], type_of(p[3]), typmod_of(p[3]), collation_of(p[3]))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
