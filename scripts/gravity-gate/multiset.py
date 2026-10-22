#!/usr/bin/env python3
"""scripts/gravity-gate/multiset.py — THE result comparator (lowerv4 plan §5.6; user 09-23: "we can change the order of
the query as long as it matches the multiset"). One classifier + one diff for regress, e2e, qfuzz and the nightly
differential; the lowerv4 rig's tests/common/multiset.rs implements the same spec and must give identical verdicts on
fixtures/multiset/*.case (agreed with M0-RIG 2026-09-23).

ORDER CLASS of a statement (text only, no planner):
  exact      EXPLAIN / SHOW / FETCH / MOVE / COPY / utility / psql meta, or an ORDER BY item that does not resolve to a result
             column (the stricter reading): rows compare as lists
  unordered  SELECT / VALUES / TABLE / WITH..SELECT with no top-level ORDER BY and no cutting LIMIT / OFFSET / FETCH, and every
             INSERT / UPDATE / DELETE / MERGE (RETURNING fixes no order): rows compare as multisets (duplicates counted)
  ordered K  a top-level ORDER BY whose items resolve to result columns K (1-based): maximal runs of equal K compare in order,
             each run as a multiset (ties free; FETCH .. WITH TIES cuts no tie)
  limited    LIMIT / OFFSET / FETCH ONLY that cuts: row count + each row a member of PG's result without the limit (pg_full,
  [K]        multiset containment); with K the key sequence is also fixed row by row. Without pg_full: strict (unordered / ordered)
Values are compared as exact text (NULL is None, never ''), so string_agg / array_agg ORDER BY, ordered-set aggregates and window
values stay exact in every class. Ties are runs of equal key TEXT: numeric 1.0 vs 1.00 is a strict false mismatch, never a pass.
Verdicts: match | mismatch | mismatch-order (equal as multisets, the class rejects the order).
FLOAT AGGREGATES (V4R-2, user 2026-09-24: "yes order dependent floats compare with tiny aggregate"): a column whose value is
an order-dependent float aggregate compares within |a - b| <= 1e-12 * max(|a|, |b|) (float8) or 4 ulps (float4); NaN,
+/-Infinity and NULL exactly, -0 = +0, an error against a value is a mismatch. Which columns: `tol` {col: 'f8' | 'f4'} from the
harness's plan (graph / targetlist) when it has one; else float_cols() reads the SQL text conservatively: the select item IS
sum / avg / var_* / variance / stddev_* / covar_* / corr / regr_* (not regr_count; no OVER), bare or cast to a float type, and
its float type is evident (that cast, a declared result type, a float cast inside the arguments, or — D432 — a bare column
argument that `schema` types float: the rig's float_schema() of the file's CREATE TABLEs). A two-argument covar_* / corr / regr_*
(regr_count is a count) is float8 whatever its argument types: its transition state is float8 either way (DAY-ORDER addendum 9,
USER 2026-10-05 ~18:52Z, main 18:54:38Z; agg_shop-0211: regr_slope over numeric and smallint at W8).
ROUNDED FLOAT AGGREGATES (D3927, main 2026-10-11T07:21:16Z under the 09-25 float ruling): a select item round(<float
aggregate>::numeric [, n]) (or CAST(.. AS numeric)) passes at a difference of one unit in its n-th decimal (kind 'r<n>', |a - b|
<= 10^-n as decimals; NULL and NaN exact): the aggregate's last bits follow the fold order, and a value on a rounding boundary
shows them as that unit (rnd-0574: a W8 variance rounded to .5994 where PostgreSQL's is .5995). The aggregate is one the rule
above takes, or one whose single argument is arithmetic (+ - * /) over columns, numbers and parentheses with a column `schema`
types float (its result is float). Anything else is exact.
KEY EQUALITY (V4R-270, D7: an equal-not-identical group / DISTINCT key outputs any member of its group): `eqk` {col: {text:
canonical text}} names the key columns whose type's `=` merges distinct texts (1.5 = 1.50, '1 mon' = '30 days', a
nondeterministic collation's 'APPLE' = 'apple'), each value's class as PostgreSQL's `=` groups them (the rig asks PG,
rig_d7.py); both sides' cells map to their class before any class / compare, so a group shows any member and nothing else
loosens (every other value and every other column stays exact text).
COLLECT ELEMENTS (V4R-315): an unordered collect aggregate's elements are a multiset (`elem`; multiset_elem.py).

  multiset.py classify '<sql>' [col ...]      print the class (and the float-tolerant columns, if any)
  multiset.py case <file.case>...              run fixtures, print each verdict (exit 1 on any disagreement)
  multiset.py --selftest                       every fixtures/multiset/*.case + the regress normaliser smoke
"""
import collections, glob, os, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(HERE, "fixtures", "multiset")
OPCH = set("+-*/<>=~!@#%^&|`?")
WORD0 = re.compile(r"[A-Za-z_\x80-\U0010ffff]")
WORDC = re.compile(r"[A-Za-z0-9_$\x80-\U0010ffff]*")
NUM = re.compile(r"(?:[0-9][0-9.]*|\.[0-9][0-9.]*)(?:[eE][+-]?[0-9]+)?")
SELECTISH, DML = {"select", "values", "table"}, {"insert", "update", "delete", "merge"}
# psql meta commands that send the query buffer (D3408): the statement ends on their line as at a top-level `;`
PSQL_SEND = {"g", "gx", "gset", "gexec", "gdesc", "crosstabview", "watch"}


def lex(s):
    """[(kind, text)]: w word (lowercased), q "quoted ident", n number, s string (prefix folded), p $n, o operator run,
    c punctuation ( ) [ ] , ; . : :: $. Comments (-- and nested /* */) are dropped."""
    out, i, n = [], 0, len(s)
    while i < n:
        c = s[i]
        if c.isspace():
            i += 1
        elif s.startswith("--", i):
            j = s.find("\n", i); i = n if j < 0 else j
        elif s.startswith("/*", i):
            d, i = 1, i + 2
            while i < n and d:
                if s.startswith("/*", i): d, i = d + 1, i + 2
                elif s.startswith("*/", i): d, i = d - 1, i + 2
                else: i += 1
        elif c == "'" or (c in "eEbBxXnN" and s[i + 1:i + 2] == "'") or (c in "uU" and s[i + 1:i + 3] == "&'"):
            j = s.index("'", i); bs = s[i:j].lower() == "e"; j += 1
            while j < n:
                if bs and s[j] == "\\": j += 2; continue
                if s[j] == "'":
                    if s[j + 1:j + 2] == "'": j += 2; continue
                    break
                j += 1
            out.append(("s", s[i:j + 1])); i = j + 1
        elif c == '"' or (c in "uU" and s[i + 1:i + 3] == '&"'):
            j = s.index('"', i) + 1
            while j < n and not (s[j] == '"' and s[j + 1:j + 2] != '"'):
                j += 2 if s[j] == '"' else 1
            out.append(("q", s[i:j + 1])); i = j + 1
        elif c == "$" and s[i + 1:i + 2].isdigit():
            j = i + 1
            while j < n and s[j].isdigit(): j += 1
            out.append(("p", s[i:j])); i = j
        elif c == "$" and re.match(r"\$(?:[A-Za-z_][A-Za-z0-9_]*)?\$", s[i:]):
            tag = re.match(r"\$(?:[A-Za-z_][A-Za-z0-9_]*)?\$", s[i:]).group(0)
            j = s.find(tag, i + len(tag)); j = n if j < 0 else j + len(tag)
            out.append(("s", s[i:j])); i = j
        elif WORD0.match(c):
            j = WORDC.match(s, i + 1).end(); out.append(("w", s[i:j].lower())); i = j
        elif NUM.match(s, i) and (c.isdigit() or s[i + 1:i + 2].isdigit()):
            j = NUM.match(s, i).end(); out.append(("n", s[i:j])); i = j
        elif s.startswith("::", i):
            out.append(("c", "::")); i += 2
        elif c in OPCH:
            j = i
            while j < n and s[j] in OPCH and not s.startswith("--", j) and not s.startswith("/*", j): j += 1
            out.append(("o", s[i:j])); i = j
        else:
            out.append(("c", c)); i += 1
    return out


def depths(toks):
    """The paren / bracket depth each token sits at (an opener at its outer depth, a closer too)."""
    d, out = 0, []
    for k, t in toks:
        if k == "c" and t in ")]": d -= 1
        out.append(d)
        if k == "c" and t in "([": d += 1
    return out


def is_w(t, *ws):
    return t[0] == "w" and t[1] in ws


def strip_outer(toks):
    while toks and toks[-1] == ("c", ";"): toks = toks[:-1]
    while toks and toks[0] == ("c", "("):
        ds = depths(toks)
        close = next((i for i in range(1, len(toks)) if ds[i] == 0 and toks[i][0] == "c" and toks[i][1] in ")]"), None)
        if close != len(toks) - 1: break
        toks = toks[1:-1]
        while toks and toks[-1] == ("c", ";"): toks = toks[:-1]
    return toks


def split0(toks, ds, sep=("c", ",")):
    parts, cur = [], []
    for t, d in zip(toks, ds):
        if t == sep and d == 0: parts.append(cur); cur = []
        else: cur.append(t)
    return parts + [cur]


def canon(toks):
    return " ".join(t[1] for t in toks)


def select_items(toks, ds):
    """[(item tokens, alias)] of the first depth-0 SELECT (DISTINCT / DISTINCT ON (..) / ALL skipped; a trailing `AS word` is
    the alias, dropped from the tokens); None when there is no depth-0 SELECT."""
    i = next((i for i, t in enumerate(toks) if ds[i] == 0 and is_w(t, "select")), None)
    if i is None: return None
    i += 1
    if i < len(toks) and is_w(toks[i], "all"): i += 1
    elif i < len(toks) and is_w(toks[i], "distinct"):
        i += 1
        if i < len(toks) and is_w(toks[i], "on"):
            i += 1
            while i < len(toks) and not (ds[i] == 0 and toks[i] == ("c", ")")): i += 1
            i += 1
    stop = {"from", "where", "group", "having", "window", "union", "intersect", "except", "order", "limit", "offset", "fetch", "for", "into"}
    j = i
    while j < len(toks) and not (ds[j] == 0 and (is_w(toks[j], *stop) or toks[j] == ("c", ";"))): j += 1
    items = []
    for it in split0(toks[i:j], ds[i:j]):
        alias = None
        if len(it) >= 2 and is_w(it[-2], "as") and it[-1][0] in "wq": it, alias = it[:-2], it[-1][1]
        items.append((it, alias))
    return items


def is_star(it):
    return it == [("o", "*")] or (len(it) >= 2 and it[-2:] == [("c", "."), ("o", "*")])


def derived_names(items):
    """Result column names from the select list when no header is given: `AS name`, a bare / qualified column's last word,
    else no name; a star makes every name unknown (None)."""
    if items is None or any(is_star(it) for it, _ in items): return None
    out = []
    for it, alias in items:
        if alias is not None: out.append(alias)
        elif len(it) == 1 and it[0][0] in "wq": out.append(it[0][1])
        elif len(it) == 3 and it[0][0] in "wq" and it[1] == ("c", ".") and it[2][0] in "wq": out.append(it[2][1])
        else: out.append(None)
    return [n[1:-1].replace('""', '"') if n and n.startswith('"') else n for n in out]


def resolve(item, cols, ncols, items):
    """(1) an ordinal; (2) one word / quoted ident naming exactly one result column; (3) the canonical text of exactly one
    select item with no star item before it. Anything else is unresolved (None)."""
    if len(item) == 1 and item[0][0] == "n" and item[0][1].isdigit():
        k = int(item[0][1])
        return k if ncols is None or 1 <= k <= ncols else None
    names = cols if cols is not None else derived_names(items)
    if len(item) == 1 and item[0][0] in "wq" and names is not None:
        name = item[0][1] if item[0][0] == "w" else item[0][1][1:-1].replace('""', '"')
        hits = [i + 1 for i, c in enumerate(names) if c == name]
        if len(hits) == 1: return hits[0]
        if hits: return None
    if items is None: return None
    ct = canon(item)
    hits = [i for i, (it, _) in enumerate(items) if canon(it) == ct]
    if len(hits) != 1 or any(is_star(it) for it, _ in items[:hits[0]]): return None
    return hits[0] + 1 if ncols is None or hits[0] < ncols else None


def classify(sql, cols=None, ncols=None):
    """(class, keys): class in exact / unordered / ordered / limited; keys a list of 1-based result columns or None."""
    toks = strip_outer(lex(sql))
    if cols is not None: ncols = len(cols)
    if not toks: return "exact", None
    ds = depths(toks)
    first = toks[0]
    if is_w(first, "with"):
        verb = next((t[1] for t, d in zip(toks, ds) if d == 0 and t[0] == "w" and t[1] in SELECTISH | DML), None)
        if verb is None: return "exact", None
        if verb in DML: return "unordered", None
    elif first[0] == "w" and first[1] in DML:
        return "unordered", None
    elif not (first == ("c", "(") or (first[0] == "w" and first[1] in SELECTISH)):
        return "exact", None
    ob = max((i for i in range(len(toks) - 1) if ds[i] == 0 and is_w(toks[i], "order") and is_w(toks[i + 1], "by")), default=None)
    cut, i = False, (ob + 2 if ob is not None else 0)
    end_items = None
    while i < len(toks):
        t = toks[i]
        if ds[i] == 0 and is_w(t, "limit", "offset", "fetch", "for"):
            if end_items is None: end_items = i
            nxt = toks[i + 1] if i + 1 < len(toks) else ("", "")
            if is_w(t, "limit") and not is_w(nxt, "all", "null"): cut = True
            if is_w(t, "offset") and not (nxt == ("n", "0") or is_w(nxt, "null")): cut = True
            if is_w(t, "fetch"):
                j = i
                while j < len(toks) and not (ds[j] == 0 and is_w(toks[j], "only", "ties")): j += 1
                if not (j < len(toks) and is_w(toks[j], "ties")): cut = True   # WITH TIES cuts no tie
        i += 1
    if ob is None:
        return ("limited", None) if cut else ("unordered", None)
    e = end_items if end_items is not None else len(toks)
    keys, items = [], select_items(toks, ds)
    for it in split0(toks[ob + 2:e], ds[ob + 2:e]):
        if len(it) >= 2 and is_w(it[-2], "nulls") and is_w(it[-1], "first", "last"): it = it[:-2]
        if it and is_w(it[-1], "asc", "desc"): it = it[:-1]
        elif len(it) >= 2 and is_w(it[-2], "using"): it = it[:-2]
        k = resolve(it, cols, ncols, items) if it else None
        if k is None: return "exact", None
        keys.append(k)
    return ("limited" if cut else "ordered"), keys


# ---------------------------------------------------------------------------------- float aggregates (V4R-2)
FLOAT_AGGS = {"sum", "avg", "variance", "var_pop", "var_samp", "stddev", "stddev_pop", "stddev_samp", "covar_pop", "covar_samp",
              "corr", "regr_avgx", "regr_avgy", "regr_intercept", "regr_r2", "regr_slope", "regr_sxx", "regr_sxy", "regr_syy"}
# addendum 9 (USER 2026-10-05): the two-argument ones accumulate float8 whatever their arguments' types (PostgreSQL casts both to
# float8 per row), so their result is order dependent even over numeric / int columns
FLOAT8_PAIR_AGGS = {a for a in FLOAT_AGGS if a.startswith(("covar_", "regr_")) or a == "corr"}


def float_type(toks, i):
    """('f4' | 'f8' | '' (another type), next index) for a type name at toks[i]."""
    t = toks[i] if i < len(toks) else ("", "")
    if is_w(t, "float4", "real"): return "f4", i + 1
    if is_w(t, "float8"): return "f8", i + 1
    if is_w(t, "double") and i + 1 < len(toks) and is_w(toks[i + 1], "precision"): return "f8", i + 2
    if is_w(t, "float"):
        if toks[i + 1:i + 2] == [("c", "(")] and i + 3 < len(toks) and toks[i + 2][0] == "n" and toks[i + 3] == ("c", ")"):
            return ("f4" if toks[i + 2][1].isdigit() and int(toks[i + 2][1]) <= 24 else "f8"), i + 4
        return "f8", i + 1
    j = i + 1
    while j < len(toks) and toks[j][0] == "w" and not is_w(toks[j], "as"): j += 1   # e.g. `timestamp with time zone`
    return "", j


def decl_kind(name):
    """A declared result type name (plan / describe) -> 'f4' | 'f8' | None."""
    k, _ = float_type(lex(name or ""), 0)
    return k or None


def float_schema(text):
    """D432: {column name: 'f8' | 'f4'} over every `CREATE [TEMP|UNLOGGED] TABLE name (col type, ...)` in `text` (a file or
    its transcript): a column name every table declares with one float type; a name any table declares otherwise is out."""
    out, bad = {}, set()
    for m in re.finditer(r"(?i)\bcreate\s+(?:(?:temp|temporary|unlogged)\s+)?table\s+(?:if\s+not\s+exists\s+)?[\w.\"]+\s*\(", text):
        toks, body, d = lex(text[m.end() - 1:m.end() + 20000]), [], 0
        for t in toks:
            d += (t == ("c", "(")) - (t == ("c", ")"))
            if d == 0: break
            body.append(t)
        body = body[1:]
        for col in split0(body, depths(body)):
            if len(col) < 2 or col[0][0] not in "wq" or is_w(col[0], "constraint", "primary", "unique", "check", "foreign", "exclude", "like"):
                continue
            name = col[0][1].strip('"').lower()
            k = float_type(col, 1)[0]
            if k and out.get(name, k) == k and name not in bad: out[name] = k
            else: bad.add(name); out.pop(name, None)
    return out


def agg_kind(it, declared=None, schema=None, arith=False):
    """'f8' / 'f4' when select item `it` is an order-dependent float aggregate with an evident float type, else None. arith
    (D3927's rounded form only): a single argument that _float_arith() types float is evidence too."""
    outer = None
    while True:
        it = strip_outer(it)
        ds = depths(it)
        cc = [i for i in range(len(it)) if it[i] == ("c", "::") and ds[i] == 0]
        if cc:   # the last top-level `::type` is the outermost cast
            k, end = float_type(it, cc[-1] + 1)
            if end != len(it) or not k: return None
            outer, it = outer or k, it[:cc[-1]]; continue
        if len(it) > 4 and is_w(it[0], "cast") and it[1] == ("c", "(") and it[-1] == ("c", ")"):
            a = next((i for i in range(2, len(it)) if ds[i] == 1 and is_w(it[i], "as")), None)
            if a is None: return None
            k, end = float_type(it, a + 1)
            if end != len(it) - 1 or not k: return None
            outer, it = outer or k, it[2:a]; continue
        break
    if len(it) < 3 or it[0][0] != "w" or it[0][1] not in FLOAT_AGGS or it[1] != ("c", "("): return None
    ds = depths(it)
    close = next(i for i in range(2, len(it)) if ds[i] == 0 and it[i] == ("c", ")")) if any(
        ds[i] == 0 and it[i] == ("c", ")") for i in range(2, len(it))) else None
    if close is None: return None
    rest = it[close + 1:]
    if len(rest) == 1 and rest[0][0] in "wq" and not is_w(rest[0], "over", "filter"): rest = []   # a bare alias
    if rest and not (is_w(rest[0], "filter") and rest[1:2] == [("c", "(")] and rest[-1] == ("c", ")")
                     and not any(is_w(t, "over") for t, d in zip(rest, ds[close + 1:]) if d == 0)):
        return None   # OVER (a window) or anything after the call: exact
    if declared is not None: return decl_kind(declared)
    if outer: return outer
    args, inner = it[2:close], None
    ads = depths(args)
    cuts = [-1] + [i for i, t in enumerate(args) if t == ("c", ",") and ads[i] == 0] + [len(args)]
    cols = [args[a + 1:b] for a, b in zip(cuts, cuts[1:])]
    if it[0][1] in FLOAT8_PAIR_AGGS and len(cols) == 2 and all(cols): return "f8"   # addendum 9: whatever the argument types
    if schema and args and all(c and c[-1][0] in "wq" and all(t[0] in "wq" or t == ("c", ".") for t in c) for c in cols):
        # bare columns (D432); the two-argument covar / corr / regr_* returned above (addendum 9 replaced M6-FOLD R2's
        # "every column float" for them)
        kinds = [schema.get(c[-1][1].strip('"').lower()) for c in cols]
        inner = kinds[0] if all(k and k == kinds[0] for k in kinds) else ("f8" if all(kinds) else None)
    if arith and schema and not inner and len(cols) == 1 and _float_arith(cols[0], schema): inner = "f8"
    for i, t in enumerate(args):
        if t == ("c", "::") or is_w(t, "as"): inner = float_type(args, i + 1)[0] or inner
        elif is_w(t, "float8", "float4") and args[i + 1:i + 2] == [("c", "(")]: inner = "f8" if t[1] == "float8" else "f4"
        if inner == "f8": break
    if not inner: return None
    return "f4" if inner == "f4" and it[0][1] == "sum" else "f8"


def _float_arith(toks, schema):
    """D3927: whether `toks` is arithmetic (+ - * /, a unary -) over column references, numbers and parentheses with at least
    one column `schema` types float: PostgreSQL's result is then float (float8 + int8, numeric * float8, ..). A call, a cast,
    a literal string or a keyword is not this form."""
    seen = []

    def operand(i):
        if i < len(toks) and toks[i] == ("o", "-"): i += 1
        if i >= len(toks): return None
        t = toks[i]
        if t == ("c", "("):
            j = expr(i + 1)
            return j + 1 if j is not None and j < len(toks) and toks[j] == ("c", ")") else None
        if t[0] == "n": return i + 1
        if t[0] in "wq":
            while i + 2 < len(toks) and toks[i + 1] == ("c", ".") and toks[i + 2][0] in "wq": i += 2
            if i + 1 < len(toks) and toks[i + 1] == ("c", "("): return None   # a function call
            seen.append(toks[i][1].strip('"').lower())
            return i + 1
        return None

    def expr(i):
        i = operand(i)
        while i is not None and i < len(toks) and toks[i][0] == "o" and toks[i][1] in ("+", "-", "*", "/"):
            i = operand(i + 1)
        return i

    return expr(0) == len(toks) and any(schema.get(c) for c in seen)


def _numeric_type(toks):
    """`numeric` / `decimal`, with or without a (precision [, scale]) typmod."""
    if not toks or not is_w(toks[0], "numeric", "decimal"): return False
    rest = toks[1:]
    return not rest or (len(rest) in (3, 5) and rest[0] == ("c", "(") and rest[-1] == ("c", ")") and rest[1][0] == "n"
                        and (len(rest) == 3 or (rest[2] == ("c", ",") and rest[3][0] == "n")))


def round_kind(it, schema=None):
    """D3927: 'r<n>' when select item `it` is round(<float aggregate>::numeric [, n]) (n an integer literal, 0 when absent; the
    cast `::numeric[(p, s)]` or CAST(.. AS numeric[(p, s)]); a bare alias after it), the aggregate one agg_kind() takes with
    the text's evidence (a declared result type is the round's, numeric) or with _float_arith(); else None."""
    it = strip_outer(it)
    if len(it) >= 2 and it[-1][0] in "wq" and it[-2] == ("c", ")"): it = it[:-1]   # a bare alias
    if len(it) < 4 or not is_w(it[0], "round") or it[1] != ("c", "(") or it[-1] != ("c", ")"): return None
    ds = depths(it)
    if any(ds[i] == 0 and it[i] == ("c", ")") for i in range(2, len(it) - 1)): return None   # round(..) is not the whole item
    args = it[2:-1]
    parts = split0(args, depths(args))
    if len(parts) > 2 or not parts[0]: return None
    n = 0
    if len(parts) == 2:
        p = parts[1]
        neg = p[:1] == [("o", "-")]
        p = p[1:] if neg else p
        if len(p) != 1 or p[0][0] != "n" or not p[0][1].isdigit(): return None
        n = -int(p[0][1]) if neg else int(p[0][1])
    x = strip_outer(parts[0])
    xd = depths(x)
    cc = [i for i in range(len(x)) if x[i] == ("c", "::") and xd[i] == 0]
    if cc:
        if not _numeric_type(x[cc[-1] + 1:]): return None
        inner = x[:cc[-1]]
    elif len(x) > 4 and is_w(x[0], "cast") and x[1] == ("c", "(") and x[-1] == ("c", ")"):
        a = next((i for i in range(2, len(x)) if xd[i] == 1 and is_w(x[i], "as")), None)
        if a is None or not _numeric_type(x[a + 1:-1]): return None
        inner = x[2:a]
    else:
        return None
    return "r%d" % n if agg_kind(inner, None, schema, arith=True) else None


def float_cols(sql, cols=None, ncols=None, types=None, schema=None):
    """{1-based column: 'f8' | 'f4' | 'r<n>'} the text reading qualifies (types: declared result type names, if the harness
    has them; 'r<n>': D3927's round(<float aggregate>::numeric, n), round_kind())."""
    toks = strip_outer(lex(sql))
    ds = depths(toks)
    if not toks or not (toks[0] == ("c", "(") or is_w(toks[0], "select", "with")): return {}
    items = select_items(toks, ds)
    if not items: return {}
    out = {}
    for k, (it, _) in enumerate(items, 1):
        if is_star(it): break   # positions after a star are unknown
        if ncols is not None and k > ncols: break
        kind = agg_kind(it, types[k - 1] if types and k <= len(types) else None, schema) or round_kind(it, schema)
        if kind: out[k] = kind
    return out


def _f4_ord(x):
    import struct
    b = struct.unpack("<i", struct.pack("<f", x))[0]
    return b if b >= 0 else -(b & 0x7FFFFFFF)


def tol_eq(a, b, kind, bound=None):
    """V4R-2's value equality for a tolerant column (texts; None = NULL). bound: ruling 3's n x eps x sum|x| for this cell
    (`Tol`, from PG's fsum_sql rows), which also passes a pair outside V4R-2's tolerance. kind 'r<n>' (D3927): decimals within
    one unit in the n-th decimal."""
    if a == b: return True
    if a is None or b is None: return False
    if kind and kind[0] == "r":
        import decimal
        try: dx, dy = decimal.Decimal(a), decimal.Decimal(b)
        except decimal.InvalidOperation: return False
        if not (dx.is_finite() and dy.is_finite()): return False   # NaN: exact
        with decimal.localcontext() as ctx:
            ctx.prec = 2000   # exact: a float8 cast to numeric has at most ~330 digits
            return abs(dx - dy) <= decimal.Decimal(1).scaleb(-int(kind[1:]))
    try: x, y = float(a), float(b)
    except ValueError: return False
    if x != x or y != y or x in (float("inf"), float("-inf")) or y in (float("inf"), float("-inf")): return False   # NaN / Inf: exact
    if x == y: return True   # -0 = +0, 1.0 = 1
    if kind == "f4":
        import struct
        try: fx, fy = struct.unpack("<f", struct.pack("<f", x))[0], struct.unpack("<f", struct.pack("<f", y))[0]
        except OverflowError: return False
        if abs(_f4_ord(fx) - _f4_ord(fy)) <= 4: return True
    elif abs(x - y) <= 1e-12 * max(abs(x), abs(y)): return True
    return bound is not None and abs(x - y) <= bound


# D2608 x ruling 3 (main 2026-10-04 02:26Z, ruled (a)): a float sum / avg column also passes within the judge's summation
# bound (JUDGE-SPEC R5, PLAN-v5 7.1 (4)): n x eps x the sum of |x| (float8 eps 2^-52, float4 2^-23), n and the sum of |x|
# PostgreSQL's own per group: fsum_sql(sql, tol) is the statement with each such select item replaced by
# `count(x)::text || '/' || sum(abs(x::float8))::text` (the judge's leg.fstat_sql form: FILTER kept, the cut dropped, every
# other item kept, so its row aligns with the result row of the same group); the harness runs it on PG (v5probe: only after
# a mismatch) and passes the rows as `fsum`. A column with no such row keeps V4R-2's tolerance alone.
FSUM_AGGS, FSUM_EPS = ("sum", "avg"), {"f8": 2.0 ** -52, "f4": 2.0 ** -23}
FSUM_CELL = re.compile(r"^(\d+)/(\S+)$")


def _qmask(text):
    """text with quoted literals, quoted identifiers and comments blanked (same length; the judge's leg.mask)."""
    out, i, n = list(text), 0, len(text)
    def blank(a, b):
        for k in range(a, b):
            if out[k] != "\n": out[k] = " "
    while i < n:
        if text.startswith("--", i):
            j = text.find("\n", i); j = n if j < 0 else j; blank(i, j); i = j
        elif text.startswith("/*", i):
            d, j = 1, i + 2
            while j < n and d:
                if text.startswith("/*", j): d += 1; j += 2
                elif text.startswith("*/", j): d -= 1; j += 2
                else: j += 1
            blank(i, j); i = j
        elif text[i] in "'\"":
            q, j = text[i], i + 1
            while j < n:
                if text[j] == q and text[j + 1:j + 2] == q: j += 2; continue
                if text[j] == q: break
                j += 1
            blank(i + 1, j); i = j + 1
        elif text[i] == "$":
            m = None if i and (text[i - 1].isalnum() or text[i - 1] in "_$" or ord(text[i - 1]) > 127) else \
                re.match(r"\$(?:[A-Za-z_\u0080-\U0010ffff][A-Za-z0-9_\u0080-\U0010ffff]*)?\$", text[i:])
            if m:
                tag = m.group(0); j = text.find(tag, i + len(tag)); j = n if j < 0 else j + len(tag)
                blank(i + len(tag), j - len(tag)); i = j
            else: i += 1
        else: i += 1
    return "".join(out)


def _words_at(m, a, b):
    """[(index, word)] of the depth-0 words of masked m[a:b]."""
    out, depth, k = [], 0, a
    while k < b:
        ch = m[k]
        if ch in "([": depth += 1
        elif ch in ")]": depth -= 1
        elif depth == 0 and (ch.isalpha() or ch == "_") and (k == a or not (m[k - 1].isalnum() or m[k - 1] in "_$.")):
            w = re.match(r"[A-Za-z_][A-Za-z0-9_$]*", m[k:b]).group(0)
            out.append((k, w.lower())); k += len(w); continue
        k += 1
    return out


def _commas_at(m, a, b):
    out, depth, last = [], 0, a
    for k in range(a, b):
        if m[k] in "([": depth += 1
        elif m[k] in ")]": depth -= 1
        elif m[k] == "," and depth == 0: out.append((last, k)); last = k + 1
    return out + [(last, b)]


def _closing(m, k):
    d = 0
    for j in range(k, len(m)):
        d += (m[j] == "(") - (m[j] == ")")
        if d == 0: return j
    return None


def fsum_sql(sql, tol):
    """ruling 3's evidence query for the `tol` columns that are exactly one sum / avg call (plain arguments, at most a
    FILTER, an alias), or None: a single SELECT with a FROM (no set operation, no DISTINCT, no INTO), its depth-0 LIMIT /
    OFFSET / FETCH dropped (a FOR UPDATE / SHARE kept)."""
    if not tol: return None
    st = sql.rstrip()
    while st.endswith(";"): st = st[:-1].rstrip()
    m = _qmask(st)
    ws = _words_at(m, 0, len(m))
    words = [w for _, w in ws]
    if words.count("select") != 1 or any(w in words for w in ("union", "intersect", "except", "into")): return None
    si = words.index("select")
    fr = next((i for i in range(si + 1, len(ws)) if ws[i][1] == "from"), None)
    if fr is None: return None
    a, b = ws[si][0] + len("select"), ws[fr][0]
    w0 = _words_at(m, a, b)
    if w0 and w0[0][1] == "distinct": return None
    if w0 and w0[0][1] == "all": a = w0[0][0] + 3
    items, hit = [], False
    for k, (x, y) in enumerate(_commas_at(m, a, b), 1):
        it, mi = st[x:y], m[x:y]
        call = None
        if tol.get(k):
            # `AS name`, or a bare name right after the call's (or its FILTER's) closing parenthesis
            al = re.search(r"(?is)\s+as\s+(\"[^\"]*\"|[A-Za-z_][\w$]*)\s*$", mi) or re.search(r"(?is)(?<=\))\s*(\"[^\"]*\"|[A-Za-z_][\w$]*)\s*$", mi)
            e, me = (it[:al.start()], mi[:al.start()]) if al else (it, mi)
            alias = it[len(e):]
            fm = re.match(r"(?is)\s*(%s)\s*\(" % "|".join(FSUM_AGGS), me)
            c = _closing(me, fm.end() - 1) if fm else None
            if c is not None and not re.search(r"(?i)\b(distinct|order\s+by|over|select)\b", me[fm.end():c]) and \
                    len(_commas_at(me, fm.end(), c)) == 1:
                rest, flt = me[c + 1:], ""
                f = re.match(r"(?is)\s*filter\s*\(", rest)
                fc = _closing(me, c + 1 + f.end() - 1) if f else None
                if not rest.strip() or (fc is not None and not me[fc + 1:].strip()):
                    flt = e[c + 1:fc + 1] if fc is not None else ""
                    arg = e[fm.end():c].strip()
                    call = "((count(%s)%s)::text || '/' || coalesce((sum(abs((%s)::float8))%s)::text, '0'))%s" % (
                        arg, flt, arg, flt, alias or " AS fsum_%d" % k)
        items.append(call if call else it.strip())
        hit |= call is not None
    if not hit: return None
    out = st[:a] + " " + ", ".join(items) + " " + st[b:]
    mo = _qmask(out)
    depth, cut = 0, None
    for k, ch in enumerate(mo):
        if ch in "([": depth += 1
        elif ch in ")]": depth -= 1
        elif depth == 0 and (k == 0 or not (mo[k - 1].isalnum() or mo[k - 1] == "_")):
            if cut is None and re.match(r"(?i)(limit|offset|fetch)\b", mo[k:]): cut = k
            if cut is not None and re.match(r"(?i)for\s+(update|share|no\s+key|key)\b", mo[k:]):
                return out[:cut].rstrip() + " " + out[k:]
    return out if cut is None else out[:cut].rstrip()


class Tol(dict):
    """{column: kind} with ruling 3's bounds: {exact-column key: {column: n x eps x sum|x|}} from PG's fsum_sql rows."""
    def __init__(self, kinds, fsum):
        super().__init__(kinds)
        self.ev = {}
        for r in fsum or []:
            r = tuple(r)
            for k, kind in kinds.items():
                mm = FSUM_CELL.match(r[k - 1] or "") if k - 1 < len(r) else None
                if not mm: continue
                try: bd = int(mm.group(1)) * FSUM_EPS.get(kind, FSUM_EPS["f8"]) * abs(float(mm.group(2)))
                except ValueError: continue
                if bd == bd and bd != float("inf"):
                    d = self.ev.setdefault(self.key(r), {}); d[k] = max(d.get(k, 0.0), bd)

    def key(self, r):
        return tuple(None if j + 1 in self else v for j, v in enumerate(r))

    def bound(self, r, k):
        return self.ev.get(self.key(r), {}).get(k)


def _mask(r, tol):
    return tuple(None if k + 1 in tol else v for k, v in enumerate(r)) + tuple(k + 1 in tol and v is None for k, v in enumerate(r))


def _fkey(r, tol):
    def f(v):
        try: x = float(v)
        except (TypeError, ValueError): return (2, 0.0, str(v))
        return (1, 0.0, v) if x != x else (0, x, "")
    return tuple(f(r[k - 1]) for k in sorted(tol) if k - 1 < len(r))


def row_eq(a, b, tol):
    bd = tol.bound if isinstance(tol, Tol) else (lambda r, k: None)
    return len(a) == len(b) and all(tol_eq(x, y, tol[k + 1], bd(a, k + 1)) if k + 1 in tol else x == y for k, (x, y) in enumerate(zip(a, b)))


def bag_eq(a, b, tol=None):
    """Multiset equality; with tol, rows equal on the exact columns pair up in float order and each pair is tol-equal."""
    if not tol: return collections.Counter(a) == collections.Counter(b)
    ga, gb = collections.defaultdict(list), collections.defaultdict(list)
    for r in a: ga[_mask(r, tol)].append(r)
    for r in b: gb[_mask(r, tol)].append(r)
    if set(ga) != set(gb): return False
    for m, ra in ga.items():
        rb = gb[m]
        if len(ra) != len(rb): return False
        if not all(row_eq(x, y, tol) for x, y in zip(sorted(ra, key=lambda r: _fkey(r, tol)), sorted(rb, key=lambda r: _fkey(r, tol)))):
            return False
    return True


def bag_within(sub, sup, tol=None):
    if not tol: return within(sub, sup)
    pool = collections.defaultdict(list)
    for r in sup: pool[_mask(r, tol)].append(r)
    for r in sorted(sub, key=lambda r: _fkey(r, tol)):
        cand = pool.get(_mask(r, tol), [])
        hit = next((i for i, c in enumerate(cand) if row_eq(r, c, tol)), None)
        if hit is None: return False
        cand.pop(hit)
    return True


# ------------------------------------------------------------------------------------------------------- the diff
def keyof(row, keys, eqo=None):   # eqo (CORR-8): {column: {text: class}} of ORDER BY key columns whose sort peers display apart
    cls = lambda k, v: eqo[k].get(v, v) if eqo and k in eqo and v is not None else v
    return tuple(cls(k, row[k - 1]) if k - 1 < len(row) else ("\0missing",) for k in keys)


def runs(rows, keys, tol=None, eqo=None):
    out = []
    for r in rows:
        k = keyof(r, keys, eqo)
        if out and (out[-1][0] == k or (tol and row_eq(out[-1][0], k, {i + 1: tol[c] for i, c in enumerate(keys) if c in tol}))):
            out[-1][1].append(r)
        else: out.append((k, [r]))
    return out


def within(sub, sup):
    s = collections.Counter(sup)
    return all(s[r] >= c for r, c in collections.Counter(sub).items())


def ordered_ok(pg, got, keys, tol=None, eqo=None):
    a, b = runs(pg, keys, tol, eqo), runs(got, keys, tol, eqo)
    kt = {i + 1: tol[c] for i, c in enumerate(keys) if c in tol} if tol else None
    return len(a) == len(b) and all((x[0] == y[0] or (kt and row_eq(x[0], y[0], kt))) and bag_eq(x[1], y[1], tol) for x, y in zip(a, b))


def compare(cls, keys, pg, got, pg_full=None, tol=None, eqo=None):
    """pg / got / pg_full: lists of rows (tuples of str | None), or ('error', sqlstate, message). tol: {1-based column:
    'f8' | 'f4'} compared within V4R-2's tolerance (float_cols / the harness's plan), every other column exactly. eqo
    (CORR-8, main 09-27): ORDER BY key columns' classes under the sort's `=`: runs and the key sequence by class, rows exact."""
    if (isinstance(pg, tuple) and pg[:1] == ("error",)) or (isinstance(got, tuple) and got[:1] == ("error",)):
        return "match" if pg == got else "mismatch"
    pg, got = [tuple(r) for r in pg], [tuple(r) for r in got]
    ms = bag_eq(pg, got, tol)
    if cls == "unordered" or (cls == "limited" and not keys and pg_full is None):
        return "match" if ms else "mismatch"
    if cls == "exact": ok = len(pg) == len(got) and all(row_eq(a, b, tol or {}) for a, b in zip(pg, got))
    elif cls == "ordered" or pg_full is None: ok = ordered_ok(pg, got, keys, tol, eqo)
    elif not keys: ok = len(pg) == len(got) and bag_within(got, pg_full, tol)
    else:
        # exact keys (no float-tolerant key column): pg_full's rows grouped by key once, in their order — the same lists the
        # per-run scan built (row_eq with no tolerance = tuple equality), without the runs x |pg_full| scan (T0-SPEED 09-24:
        # 750 of a W0 analysis's 816 s went to that scan)
        by_key = collections.defaultdict(list)
        if not (tol and any(c in tol for c in keys)):
            for r in pg_full: by_key[keyof(tuple(r), keys, eqo)].append(tuple(r))
        full_run = lambda k: by_key.get(k, [])
        kt = {i + 1: tol[c] for i, c in enumerate(keys) if c in tol} if tol else {}
        ok = (len(pg) == len(got) and all(row_eq(keyof(a, keys, eqo), keyof(b, keys, eqo), kt) for a, b in zip(pg, got))
              and all(bag_within(rs, full_run(k), tol) for k, rs in runs(got, keys, tol, eqo))) if not kt else (
            len(pg) == len(got) and all(row_eq(keyof(a, keys, eqo), keyof(b, keys, eqo), kt) for a, b in zip(pg, got))
              and all(bag_within(rs, [tuple(r) for r in pg_full if row_eq(keyof(tuple(r), keys, eqo), k, kt)], tol) for k, rs in runs(got, keys, tol, eqo)))
    return "match" if ok else ("mismatch-order" if ms else "mismatch")


def key_classes(rows, eqk):
    """V4R-270: rows (or an error tuple) with each eqk column's cell mapped to its equality class's text."""
    if not eqk or not isinstance(rows, list): return rows
    return [tuple(eqk[k + 1].get(v, v) if k + 1 in eqk and v is not None else v for k, v in enumerate(r)) for r in rows]


exec(compile(open(os.path.join(HERE, "multiset_elem.py"), encoding="utf-8").read(), os.path.join(HERE, "multiset_elem.py"), "exec"), globals())   # V4R-315


def verdict(sql, pg, got, cols=None, pg_full=None, tol=None, types=None, schema=None, eqk=None, elem=None, eqo=None, fsum=None):
    """classify + compare in one call: (verdict, class, keys). tol: the plan's float-tolerant columns; else float_cols()
    over the SQL text (types: declared result type names, when the harness has them). eqk: V4R-270's key classes. elem: V4R-315.
    fsum: PG's rows of fsum_sql(sql, tol) (ruling 3's per-group bound for the sum / avg columns, D2608)."""
    pg, got, pg_full = elem_rows(pg, elem), elem_rows(got, elem), elem_rows(pg_full, elem)
    pg, got, pg_full = key_classes(pg, eqk), key_classes(got, eqk), key_classes(pg_full, eqk)
    rows = [r for side in (pg, got) if isinstance(side, list) for r in side]
    n = len(rows[0]) if rows else None
    cls, keys = classify(sql, cols, n)
    if tol is None: tol = float_cols(sql, cols, len(cols) if cols is not None else n, types, schema)
    if tol and fsum: tol = Tol(tol, key_classes(elem_rows([tuple(r) for r in fsum], elem), eqk))   # keyed as pg's rows are
    return compare(cls, keys, pg, got, pg_full, tol, eqo), cls, keys


def qfuzz_verdict(a, b, ordered, sep):
    """gravity-qfuzz.py's seam: a / b = pgrust / oracle row lines; `ordered` = the SQL text (or a bool: True = total order)."""
    rows = lambda ls: [tuple(l.split(sep)) for l in ls]
    if isinstance(ordered, str): v = verdict(ordered, rows(b), rows(a))[0]
    else: v = compare("exact" if ordered else "unordered", None, rows(b), rows(a))
    return {"match": "ok", "mismatch": "MISMATCH", "mismatch-order": "MISMATCH-ORDER"}[v]


def order_digest(sql, rows, cols=None):
    """The nightly's order digest: md5 over the tie-group sequence (each group's key + its sorted rows), so rows that trade
    places inside a tie (TPC-H Q11's `value DESC`) keep the digest; '-' when the class fixes no order."""
    import hashlib
    cls, keys = classify(sql, cols, len(rows[0]) if rows else None)
    if cls in ("unordered",) or (cls == "limited" and not keys): return "-"
    rows = [tuple(r) for r in rows]
    groups = [[r] for r in rows] if cls == "exact" else [sorted(g, key=repr) for _, g in runs(rows, keys)]
    return hashlib.md5(repr(groups).encode()).hexdigest()[:8]


def skip_mark(out_path):
    """regress-order-compare.py's CASCADE mark: 'skip query refused: <statement> (<refusal>)' when the .out shows a skip
    query (a statement feeding \\gset, then `\\if :<var>`) refused; else None (regress-residual-off.py attributes it)."""
    try: ls = open(out_path, encoding="utf-8", errors="surrogateescape").read().split("\n")
    except OSError: return None
    for i, l in enumerate(ls):
        m = re.match(r"^ERROR:  gravity: refused \((.*)\)$", l)
        if m and any("\\gset" in x for x in ls[max(0, i - 3):i]) and any(re.match(r"^\s*\\if\s+:", x) for x in ls[i + 1:i + 6]):
            return f"skip query refused: {next(x for x in reversed(ls[max(0, i - 3):i]) if chr(92) + 'gset' in x).strip()} ({m.group(1)})"
    return None


BODY_SKIP = re.compile(r"^(Time:|Timing is|Tuples only|Output format|Pager|psql:|(ERROR|NOTICE|WARNING|INFO|DETAIL|HINT|CONTEXT|LOCATION|STATEMENT):)")


def nightly_order_wrong(sql, out, ref_out, fallback, sep="|"):
    """gravity-nightly/compare.py's seam: True when the night's rows break the order the SQL fixes, per tie group, against
    the reference's rows (gravity-bar-dryrun.sh out files: unaligned, tuples only); `fallback` (the old whole-order digest
    verdict) when a statement or an out file is missing."""
    if not sql or not out or not ref_out or not os.path.exists(out) or not os.path.exists(ref_out): return fallback
    body = lambda p: [tuple(l.split(sep)) for l in open(p, errors="replace").read().split("\n") if l and not BODY_SKIP.match(l)]
    a, b = body(ref_out), body(out)
    cls, keys = classify(sql, None, len(a[0]) if a else None)
    return compare(cls, keys, a, b) == "mismatch-order"


# ------------------------------------------------------------------------------------ the regress / e2e seam (--auto)
def sends(t):
    """True when the tokens hold a psql meta command that sends the query buffer (PSQL_SEND; lex keeps `\\` outside quotes)."""
    if isinstance(t, str): t = lex(t)
    return any(x == ("c", "\\") and y[0] == "w" and y[1] in PSQL_SEND for x, y in zip(t, t[1:]))


def sql_statements(path, m):
    """[(annotation (mode, args) | None, verbatim statement lines, text, nxt)] in file order (psql echoes every input line;
    nxt = the next sql line, where a ruled block ends). A statement carrying a psql meta command or two statements on one
    line gets text None: never auto-classified (exact). A line with a meta command that sends the buffer (\\g, \\gset,
    \\gexec, `\\bind .. \\g`, ..: PSQL_SEND) ends the statement as psql does (D3408: `SELECT .. AS x \\gset` swallowed the
    statements up to the next `;` into one exact block, so their order class and annotations were never applied)."""
    lines = open(path, encoding="utf-8", errors="surrogateescape").read().split("\n")
    out, i, ann = [], 0, None
    while i < len(lines):
        ln = lines[i]
        a = m.ANNOT_RE.match(ln)
        if a:
            ann = (a.group(1), a.group(2)); i += 1; continue
        if not ln.strip() or ln.lstrip().startswith("--") or ln.lstrip().startswith("\\"):
            i += 1; continue
        j = i
        while True:
            t = lex("\n".join(lines[i:j + 1]))
            if j + 1 >= len(lines) or (t and t[-1] == ("c", ";") and depths(t)[-1] == 0): break
            if sends(t): break
            j += 1
        ds = depths(t)
        mixed = ("c", "\\") in t or sum(1 for x, d in zip(t, ds) if x == ("c", ";") and d == 0) > 1
        out.append((ann, lines[i:j + 1], None if mixed else "\n".join(lines[i:j + 1]), m.next_sql_line(lines, j + 1)))
        ann, i = None, j + 1
    return out


def may_return_rows(text):
    t = strip_outer(lex(text)) if text else None
    if not t: return False
    if t[0] == ("c", "(") or is_w(t[0], "select", "values", "table", "with"): return True
    return t[0][0] == "w" and t[0][1] in DML and any(d == 0 and is_w(x, "returning") for x, d in zip(t, depths(t)))


def canon_cell(v, kind):
    try: x = float(v)
    except ValueError: return v
    if x != x or x in (float("inf"), float("-inf")): return v
    return format(x + 0.0, ".12e" if kind == "f8" else ".7e")   # + 0.0: -0 = +0


def flat_block(lines, end, r, spans, tol):
    """V4R-2 in the regress / e2e text seam: the block's cells stripped and re-joined (`a | b`, so a longer float on one side
    shifts no padding) and each float-tolerant cell rounded to 13 significant digits (float8; 8 for float4). Equal after the
    rounding implies within the tolerance, so the text diff never passes a pair the tolerance fails; a pair inside it that
    rounds apart still shows (the exact side, the safe one)."""
    cut = lambda l: [l[s:(e if i < len(spans) - 1 else None)].strip() for i, (s, e) in enumerate(spans)]
    lines[end], lines[end + 1] = " | ".join(cut(lines[end])), "-" * 3
    for i in range(end + 2, r):
        lines[i] = " | ".join(canon_cell(c, tol[k + 1]) if k + 1 in tol else c for k, c in enumerate(cut(lines[i])))


def auto_normalize(m, sql_path, anchors):
    """A drop-in for regress-order-compare.normalize: every row-returning statement gets its order class; an annotation still
    wins (rowsort / stable-tie / ruled keep their meaning). Both sides resolve the same way (annotation echoes stripped,
    statements found in file order at a statement boundary), so a mis-resolution can only leave a block exact."""
    local, plan, ptr = list(anchors), [], 0
    nulls = m.null_texts(open(sql_path, encoding="utf-8", errors="surrogateescape").read().split("\n")) if hasattr(m, "null_texts") else ("",)
    for ann, stmt, text, nxt in sql_statements(sql_path, m):
        k = None
        if ann:
            k = next((x for x in range(ptr, len(anchors)) if anchors[x].stmt == stmt and anchors[x].mode == ann[0]), None)
            if k is None: local.append(m.Anchor(ann[0], ann[1], stmt, nxt, nulls)); k = len(local) - 1
            else: ptr = k + 1
        plan.append((local[k] if k is not None else None, k, stmt, text))
    tables = file_tables(sql_path)

    def normalize(out_path, _anchors, verbose=False, refused=frozenset(), blocks=None):
        try:
            lines = [l for l in open(out_path, encoding="utf-8", errors="surrogateescape").read().split("\n") if not m.ANNOT_RE.match(l)]
        except OSError as e:
            m.warn(f"cannot read {out_path}: {e}"); return None
        pos, masked = 0, []
        for ann, k, stmt, text in plan:
            if ann is None and not may_return_rows(text): continue
            at = m.find_stmt(lines, stmt, pos)
            while at >= 0 and not m.at_statement_boundary(lines, at): at = m.find_stmt(lines, stmt, at + 1)
            if at < 0: continue
            end = pos = at + len(stmt)
            if ann is not None and ann.mode == "ruled":
                stop = m.ruled_block_end(lines, end, ann.nxt) if m.ruled_applicable(ann, refused) else None
                if stop is not None:
                    masked.append((end, stop, k)); pos = stop
                    if blocks is not None and k < len(anchors): blocks[k] = m.block_content(lines, end, stop)
                continue
            r = m.table_extent(lines, end)
            if r is None: continue
            # TD-43: an any-value statement's picks masked (its rowsort sorts them), then its own order class as any statement;
            # its membership check (any-value-check) compares as any statement
            av = ann is not None and ann.mode == "any-value" and m.any_value_table(lines, end, ann, f"statement at line {end + 1} of {out_path}")
            if av and av[1]: pos = r + 1; continue
            if ann is not None and ann.mode not in ("any-value", "any-value-check"):
                m.sort_block(lines, end, ann.mode, ann.args, f"statement at line {end + 1} of {out_path}", verbose)
            elif text is not None:   # a statement holding a psql meta command: exact
                spans = m.col_spans(lines[end + 1])
                cols = [lines[end][s:e].strip() for s, e in spans]
                cls, keys = classify(text, cols)
                elem_block(lines, end, r, spans, collect_cols(text, len(cols), tables))
                if cls == "unordered" or (cls == "limited" and not keys): lines[end + 2:r] = sorted(lines[end + 2:r])
                elif cls in ("ordered", "limited"): lines[end + 2:r] = m.tie_sort_rows(lines[end + 2:r], spans, keys)
                tol = float_cols(text, cols)
                if tol: flat_block(lines, end, r, spans, tol)
            pos = r + 1
        return m.apply_masks(lines, masked, local)
    return normalize


# --------------------------------------------------------------------------------------------------- fixtures
def unescape(v):
    if v == "\\N": return None
    return re.sub(r"\\(.)", lambda x: {"t": "\t", "n": "\n", "r": "\r"}.get(x.group(1), x.group(1)), v)


def read_case(path):
    c, sec = {"sql": [], "cols": None, "class": None, "verdict": None, "tol": None, "types": None, "schema": None}, None
    for ln in open(path, encoding="utf-8").read().split("\n"):
        if not ln.strip() or (sec is None and ln.startswith("#")): continue
        if ln.strip() in ("[pg]", "[pg_full]", "[got]", "[pg_error]", "[got_error]"):
            sec = ln.strip()[1:-1]; c[sec] = []; continue
        if sec is None:
            k, _, v = ln.partition(":")
            v = v[1:] if v.startswith(" ") else v
            if k == "sql": c["sql"].append(v)
            elif k == "cols": c["cols"] = v.split("\t")
            elif k == "types": c["types"] = v.split("\t")   # declared result types (V4R-2)
            elif k == "schema": c["schema"] = float_schema(v)   # the file's CREATE TABLEs (D432)
            elif k == "tol": c["tol"] = {int(x.split(":")[0]): x.split(":")[1] for x in v.split(",") if x.strip()}   # the plan path: 1:f8,2:f4
            elif k in ("elem", "tables"): case_key(c, k, v)   # V4R-315 (multiset_elem.py)
            elif k in ("eqk", "eqo"): c.setdefault(k, {}).setdefault(int(v.split("\t")[0]), {})[unescape(v.split("\t")[1])] = unescape(v.split("\t")[2])   # V4R-270: col<TAB>text<TAB>class
            else: c[k] = v.strip()
        elif sec.endswith("_error"):
            c[sec] = ("error",) + tuple(ln.split("\t", 1))
        else:
            c[sec].append(tuple(unescape(v) for v in ln.split("\t")))
    c["sql"] = "\n".join(c["sql"])
    return c


def run_case(path):
    """(ok, line): the computed class and verdict against the case's `class:` / `verdict:`."""
    c = read_case(path)
    pg = c.get("pg_error") or c.get("pg", [])
    got = c.get("got_error") or c.get("got", [])
    v, cls, keys = verdict(c["sql"], pg, got, c["cols"], c.get("pg_full"), c.get("tol"), c.get("types"), c.get("schema"), c.get("eqk"), case_elem(c), c.get("eqo"))
    shown = cls + (" " + ",".join(map(str, keys)) if keys else "")
    ok = shown == c["class"] and v == c["verdict"]
    return ok, f"{'ok  ' if ok else 'FAIL'} {os.path.basename(path)[:-5]}: class {shown} (want {c['class']}), verdict {v} (want {c['verdict']})"


def regress_smoke():
    """regress-order-compare.py --auto on a synthetic pg_regress pair: legal reorders pass, an order the SQL fixes does not."""
    import subprocess, tempfile
    roc = os.path.join(HERE, "..", "regress-order-compare.py")
    if "import multiset" not in (open(roc).read() if os.path.exists(roc) else ""):
        print("multiset: regress-order-compare.py has no multiset seam in this tree (the regress seam check is skipped)"); return []
    sql = ["CREATE TABLE t (g int, x text);", "-- pgrust:rowsort", "SELECT x FROM t WHERE g = 9;", "SELECT g, x FROM t;",
           "SELECT g, x FROM t ORDER BY g;", "EXPLAIN (COSTS OFF) SELECT * FROM t;"]
    tab = lambda h, sep, rows: [h, sep] + rows + [f"({len(rows)} rows)", ""]
    def out(rs, un, od, ex, annot):
        return "\n".join(["CREATE TABLE t (g int, x text);", "CREATE TABLE"] + (["-- pgrust:rowsort"] if annot else [])
                         + ["SELECT x FROM t WHERE g = 9;"] + tab(" x ", "---", rs) + ["SELECT g, x FROM t;"] + tab(" g | x ", "---+---", un)
                         + ["SELECT g, x FROM t ORDER BY g;"] + tab(" g | x ", "---+---", od)
                         + ["EXPLAIN (COSTS OFF) SELECT * FROM t;"] + tab("  QUERY PLAN   ", "---------------", ex)) + "\n"
    exp = out([" a", " b"], [" 1 | a", " 2 | b"], [" 1 | a", " 1 | b", " 2 | c"], [" Seq Scan on t", "   Filter: x"], False)
    cases = [("legal reorders", out([" b", " a"], [" 2 | b", " 1 | a"], [" 1 | b", " 1 | a", " 2 | c"], [" Seq Scan on t", "   Filter: x"], True), 0),
             ("tie groups out of order", out([" a", " b"], [" 1 | a", " 2 | b"], [" 2 | c", " 1 | a", " 1 | b"], [" Seq Scan on t", "   Filter: x"], True), 1),
             ("EXPLAIN lines swapped", out([" a", " b"], [" 1 | a", " 2 | b"], [" 1 | a", " 1 | b", " 2 | c"], ["   Filter: x", " Seq Scan on t"], True), 1)]
    bad, d = [], tempfile.mkdtemp(prefix="ms-regress-")
    open(os.path.join(d, "t.sql"), "w").write("\n".join(sql) + "\n"); open(os.path.join(d, "exp.out"), "w").write(exp)
    for name, act, want in cases:
        open(os.path.join(d, "act.out"), "w").write(act)
        for auto, w in ((True, want), (False, 1 if name == "legal reorders" else want)):
            rc = subprocess.run([sys.executable, roc, "--sql", os.path.join(d, "t.sql"),
                                 "--actual", os.path.join(d, "act.out"), "--expected", os.path.join(d, "exp.out")] + (["--auto"] if auto else []),
                                capture_output=True, text=True).returncode
            if rc != w: bad.append(f"FAIL regress seam: {name} {'--auto' if auto else 'annotations only'}: rc {rc}, want {w}")
    # V4R-2 through the seam: a float8 sum that differs in the last bits (and in width) passes, an integer column does not
    fsql = ["SELECT g, sum(x::float8) FROM r GROUP BY g;"]
    fexp = "\n".join(["SELECT g, sum(x::float8) FROM r GROUP BY g;"] + tab(" g |        sum         ", "---+--------------------",
                      [" 1 | -643677878.4940954", " 2 |                1.5"])) + "\n"
    open(os.path.join(d, "f.sql"), "w").write("\n".join(fsql) + "\n"); open(os.path.join(d, "fexp.out"), "w").write(fexp)
    for name, rows, want in (("float sum, last bits", [" 2 | 1.5000000000000002", " 1 | -643677878.4940783"], 0),
                             ("float sum, an integer key changed", [" 3 | 1.5", " 1 | -643677878.4940783"], 1),
                             ("float sum, beyond 1e-12", [" 2 | 1.5", " 1 | -643677878.4950783"], 1)):
        open(os.path.join(d, "fact.out"), "w").write("\n".join(["SELECT g, sum(x::float8) FROM r GROUP BY g;"]
                                                              + tab(" g |        sum         ", "---+--------------------", rows)) + "\n")
        rc = subprocess.run([sys.executable, roc, "--sql", os.path.join(d, "f.sql"), "--actual", os.path.join(d, "fact.out"),
                             "--expected", os.path.join(d, "fexp.out"), "--auto"], capture_output=True, text=True).returncode
        if rc != want: bad.append(f"FAIL regress seam: {name}: rc {rc}, want {want}")
    bad += elem_smoke(roc, d, tab)   # V4R-315 through the seam (multiset_elem.py)
    __import__("shutil").rmtree(d, ignore_errors=True)
    for b in bad: print(b)
    return bad


def float_smoke():
    """V4R-2's required cases: a W16 sum over 1e6 random float8s that differs from the serial sum in the last bits matches;
    NaN vs a value, a round()::numeric flip and an integer sum difference do not."""
    import random
    rnd = random.Random(20260924)
    xs = [rnd.uniform(-1e6, 1e6) for _ in range(1_000_000)]
    serial, w16 = 0.0, 0.0
    for x in xs: serial += x
    parts = [0.0] * 16
    for i, x in enumerate(xs): parts[i % 16] += x
    for p in parts: w16 += p
    a, b = repr(serial), repr(w16)
    bad = []
    def want(name, got_v, v):
        if got_v != v: bad.append(f"FAIL float: {name}: {got_v}, want {v}")
    want("serial and W16 sums differ in the last bits", a != b and abs(serial - w16) <= 1e-9 * abs(serial), True)
    want("W16 sum, the plan's tol", verdict("SELECT sum(x) FROM r", [(a,)], [(b,)], tol={1: "f8"})[0], "match")
    want("W16 sum, a float8 cast in the text", verdict("SELECT sum(x::float8) FROM r", [(a,)], [(b,)])[0], "match")
    want("W16 sum, a declared float8 result", verdict("SELECT sum(x) FROM r", [(a,)], [(b,)], types=["double precision"])[0], "match")
    want("W16 sum, no type evidence: exact", verdict("SELECT sum(x) FROM r", [(a,)], [(b,)])[0], "mismatch")
    want("W16 sum grouped, unordered", verdict("SELECT g, sum(x::float8) FROM r GROUP BY g", [("1", a), ("2", "1.5")], [("2", "1.5"), ("1", b)])[0], "match")
    want("NaN vs a value", verdict("SELECT sum(x) FROM r", [("NaN",)], [(a,)], tol={1: "f8"})[0], "mismatch")
    # D3927 (main 2026-10-11T07:21:16Z): a round()::numeric flip by one unit in its last decimal passes (it replaced V4R-2's
    # "a round()::numeric flip is a mismatch"); two units, a wider gap at that decimal, NaN and NULL do not
    want("D3927: round()::numeric flip by one unit", verdict("SELECT round(sum(x::float8)::numeric, 2) FROM r", [("2.50",)], [("2.49",)])[0], "match")
    want("D3927: round()::numeric by two units", verdict("SELECT round(sum(x::float8)::numeric, 2) FROM r", [("2.50",)], [("2.48",)])[0], "mismatch")
    want("D3927: round()::numeric, the r2 column", float_cols("SELECT round(sum(x::float8)::numeric, 2) FROM r"), {1: "r2"})
    want("D3927: inside and outside one unit", [tol_eq("2.50", y, "r2") for y in ("2.51", "2.49", "2.511", "2.489", "NaN", None)],
         [True, True, False, False, False, False])
    want("D3927: n absent = 0, n negative, CAST, a typmod, an alias, FILTER, a declared float column",
         float_cols("SELECT round(avg(x::float8)::numeric), round(sum(x::float8)::numeric, -2), round(CAST(stddev(f) AS numeric), 3), "
                    "round(variance(f)::numeric(30, 6), 4) v, round(sum(f) FILTER (WHERE f > 0)::decimal, 1) FROM r", schema={"f": "f8"}),
         {1: "r0", 2: "r-2", 3: "r3", 4: "r4", 5: "r1"})
    want("D3927: n = -2 is one unit of 100", [tol_eq("1200", y, "r-2") for y in ("1300", "1400")], [True, False])
    want("D3927: arithmetic over a float column is float evidence", float_cols(
        "SELECT round(variance((e.e_val + e.e_id))::numeric, 4), round(avg(-e_val * 2 / (e_id - 1))::numeric, 2) FROM ev e",
        schema={"e_val": "f8"}), {1: "r4", 2: "r2"})
    want("D3927: these stay exact (no float evidence, a call or cast in the argument, a window, no numeric cast, a cast "
         "outside the round, a float round, a non-literal n, an integer sum)",
         float_cols("SELECT round(variance(e_id)::numeric, 4), round(sum(abs(e_val))::numeric, 2), round(sum(e_val::int + e_id)::numeric, 2), "
                    "round(sum(e_val) OVER ()::numeric, 2), round(sum(e_val), 2), round(sum(e_val)::numeric, 2)::text, round(sum(e_val)::float8), "
                    "round(sum(e_val)::numeric, k), round(sum(e_id + 1)::numeric, 2), round(sum(e_val)::numeric, 2) + 1 FROM ev",
                    schema={"e_val": "f8"}), {})
    want("D3927: a declared numeric result type does not hide the rule", float_cols(
        "SELECT g, round(variance(e_val)::numeric, 4) FROM ev GROUP BY g", types=["integer", "numeric"], schema={"e_val": "f8"}), {2: "r4"})
    want("integer sum difference", verdict("SELECT sum(i) FROM r", [("1000000",)], [("1000001",)], types=["bigint"])[0], "mismatch")
    want("integer sum, float tol claimed but exact text differs by 1", tol_eq("1000000", "1000001", "f8"), False)
    want("-0 = +0", tol_eq("-0", "0", "f8"), True)
    want("Infinity vs 1.7976931348623157e+308", tol_eq("Infinity", "1.7976931348623157e+308", "f8"), False)
    want("NULL vs 0", tol_eq(None, "0", "f8"), False)
    want("an error vs a value", compare("unordered", None, ("error", "22003", "value out of range"), [(a,)], tol={1: "f8"}), "mismatch")
    want("1e-12 relative: inside", tol_eq("1", "1.0000000000009", "f8"), True)
    want("1e-12 relative: outside", tol_eq("1", "1.000000000002", "f8"), False)
    want("float4: 4 ulps", tol_eq("1", repr(1 + 4 * 2 ** -23), "f4"), True)
    want("float4: 5 ulps", tol_eq("1", repr(1 + 5 * 2 ** -23), "f4"), False)
    want("sum(real) is float4, avg(real) float8", (float_cols("SELECT sum(x::real), avg(x::real), stddev(x::float4) FROM r")), {1: "f4", 2: "f8", 3: "f8"})
    want("windows and regr_count stay exact", float_cols("SELECT sum(x::float8) OVER (), regr_count(y::float8, x::float8) FROM r"), {})
    # addendum 9 (USER 2026-10-05): two-argument covar_* / corr / regr_* are float8 whatever the argument types
    want("addendum 9: regr_* / covar_* / corr over numeric / int columns are f8",
         float_cols("SELECT regr_slope(total, item_count), covar_samp(n, i) FILTER (WHERE i > 0), corr(a + 1, b), regr_r2(x, y) AS r2 FROM t",
                    schema={"x": "f8"}), {1: "f8", 2: "f8", 3: "f8", 4: "f8"})
    want("addendum 9: OVER, regr_count, sum over numeric and a one-argument call stay exact",
         float_cols("SELECT corr(a, b) OVER (), regr_count(a, b), sum(total), regr_slope(a) FROM t", schema={}), {})
    q9 = "SELECT regr_slope(total, item_count) AS slope, regr_intercept(total, item_count) AS intercept, regr_r2(total, item_count) AS r2 FROM orders"
    pg9 = [("0.001999999999999973", "250.97999999999996", "2.4000000009599255e-10")]
    want("addendum 9: agg_shop-0211 W8 (box 65 18:50Z values) matches",
         verdict(q9, pg9, [("0.002000000000000152", "250.98", "2.4000000009603603e-10")], schema={})[0], "match")
    want("addendum 9: still 1e-12 relative, not looser",
         verdict(q9, pg9, [("0.002000000000010152", "250.98", "2.4000000009603603e-10")], schema={})[0], "mismatch")
    want("an outer float cast is evidence", float_cols("SELECT avg(x)::float8, CAST(sum(y) AS double precision) FROM r"), {1: "f8", 2: "f8"})
    want("a numeric cast is not", float_cols("SELECT sum(x::float8)::numeric FROM r"), {})
    want("ordered by the float sum", verdict("SELECT g, sum(x::float8) AS s FROM r GROUP BY g ORDER BY s", [("1", "1.5"), ("2", a)], [("1", "1.5"), ("2", b)])[:2], ("match", "ordered"))
    want("a bare alias", float_cols("SELECT g, sum(x::float8) s FROM r GROUP BY g"), {2: "f8"})
    # D2608 x ruling 3 (main 10-04 02:26Z): window_frames-0091's outer float4 sum over 100,003 window values: PG's in-order
    # float4 9.7006e+06, Gravity's 9.697707e+06 (the exact sum 9697708); n x 2^-23 x sum|x| = 115,644 > 2,893
    q = "SELECT count(*), sum(w) FROM (SELECT sum(f4) OVER (ORDER BY id ROWS 20 PRECEDING) w FROM tb) s"
    pgr, ours = [("100003", "9.7006e+06")], [("100003", "9.697707e+06")]
    want("D2608: 4 float4 ulps alone fail it", verdict(q, pgr, ours, tol={2: "f4"})[0], "mismatch")
    want("D2608: fsum_sql", fsum_sql(q, {2: "f4"}), "SELECT count(*), ((count(w))::text || '/' || coalesce((sum(abs((w)::float8)))::text, '0')) AS fsum_2 FROM (SELECT sum(f4) OVER (ORDER BY id ROWS 20 PRECEDING) w FROM tb) s")
    want("D2608: inside ruling 3's bound", verdict(q, pgr, ours, tol={2: "f4"}, fsum=[("100003", "100003/9697708")])[0], "match")
    want("D2608: the bound is per group's n and sum|x|", verdict(q, pgr, ours, tol={2: "f4"}, fsum=[("100003", "1000/9697708")])[0], "mismatch")
    want("D2608: another count's row is no evidence", verdict(q, pgr, ours, tol={2: "f4"}, fsum=[("100002", "100003/9697708")])[0], "mismatch")
    g = "SELECT g, sum(x::float8) AS s FROM r GROUP BY g ORDER BY g LIMIT 2"
    want("fsum_sql: alias kept, cut dropped", fsum_sql(g, {2: "f8"}), "SELECT g, ((count(x::float8))::text || '/' || coalesce((sum(abs((x::float8)::float8)))::text, '0')) AS s  FROM r GROUP BY g ORDER BY g")
    want("ruling 3 per group: g=1 inside its bound, g=2 not",
         [verdict(g, [("1", "1"), ("2", "1")], [("1", "1.000001"), ("2", y)], fsum=[("1", "4/1e12"), ("2", "4/1")])[0] for y in ("1.000001", "1")],
         ["mismatch", "match"])
    want("fsum_sql: FILTER kept, bare alias", fsum_sql("SELECT g, sum(x) FILTER (WHERE x > 0) s FROM t GROUP BY g", {2: "f8"}),
         "SELECT g, ((count(x) FILTER (WHERE x > 0))::text || '/' || coalesce((sum(abs((x)::float8)) FILTER (WHERE x > 0))::text, '0')) s  FROM t GROUP BY g")
    want("fsum_sql: no evidence form for a cast call, DISTINCT, a set operation, a window, var / stddev",
         [fsum_sql(x, {1: "f8"}) for x in ("SELECT sum(x)::float8 FROM t", "SELECT DISTINCT sum(x) FROM t", "SELECT sum(x) FROM t UNION ALL SELECT 1",
                                         "SELECT sum(x) OVER () FROM t", "SELECT stddev(x::float8) FROM t", "SELECT sum(DISTINCT x) FROM t")], [None] * 6)
    return bad


def send_smoke():
    """D3408 through the seam: a line with a buffer-sending meta command (\\gset, `\\bind .. \\g`) ends its statement, so the next
    statement keeps its own order class (a legal reorder passes) and its annotation."""
    import subprocess, tempfile
    roc = os.path.join(HERE, "..", "regress-order-compare.py")
    if "import multiset" not in (open(roc).read() if os.path.exists(roc) else ""):
        print("multiset: regress-order-compare.py has no multiset seam in this tree (the send seam check is skipped)"); return []
    import importlib.util
    spec = importlib.util.spec_from_file_location("roc_send", roc); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
    sql = ["SELECT 7 AS k \\gset", "SELECT g, x FROM t;", "SELECT g FROM t WHERE g = $1 \\bind 1 \\g", "-- pgrust:rowsort", "SELECT x FROM t;"]
    d = tempfile.mkdtemp(prefix="ms-send-"); f = os.path.join(d, "s.sql")
    open(f, "w").write("\n".join(sql) + "\n")
    bad, got = [], [(a and a[0], st, tx is not None) for a, st, tx, _ in sql_statements(f, m)]
    want = [(None, sql[0:1], False), (None, sql[1:2], True), (None, sql[2:3], False), ("rowsort", sql[4:5], True)]
    if got != want: bad.append(f"FAIL send seam (D3408): statements {got}")
    tab = lambda h, sep, rows: [h, sep] + rows + [f"({len(rows)} rows)", ""]
    out = lambda r1, r2: "\n".join([sql[0], sql[1]] + tab(" g | x ", "---+---", r1) + [sql[2]] + tab(" g ", "---", [" 1"])
                                   + [sql[4]] + tab(" x ", "---", r2)) + "\n"
    open(os.path.join(d, "exp.out"), "w").write(out([" 1 | a", " 2 | b"], [" a", " b"]))
    for name, act, want_rc in (("a legal reorder after \\gset", out([" 2 | b", " 1 | a"], [" b", " a"]), 0),
                               ("a changed row after \\gset", out([" 2 | c", " 1 | a"], [" a", " b"]), 1)):
        open(os.path.join(d, "act.out"), "w").write(act)
        rc = subprocess.run([sys.executable, roc, "--sql", f, "--actual", os.path.join(d, "act.out"), "--expected", os.path.join(d, "exp.out"),
                             "--auto"], capture_output=True, text=True).returncode
        if rc != want_rc: bad.append(f"FAIL send seam (D3408): {name}: rc {rc}, want {want_rc}")
    __import__("shutil").rmtree(d, ignore_errors=True)
    for b in bad: print(b)
    return bad


def selftest():
    # schema/ (D432), elem/ (V4R-315) and eqo/ (CORR-8): keys the Rust mirror in lowerv4 tests/common does not read
    files = [f for d in ("", "float", "schema", "elem", "eqo") for f in sorted(glob.glob(os.path.join(FIXTURES, d, "*.case")))]
    bad = [line for ok, line in map(run_case, files) if not ok]
    for line in bad: print(line)
    assert files, f"no fixtures under {FIXTURES}"
    assert classify("select 1 as a order by 1 limit 3", ["a"]) == ("limited", [1])
    assert lex("E'a\\'b' x") == [("s", "E'a\\'b'"), ("w", "x")] and lex("a::int $1 <>--c\n/* /* */ */b") == [
        ("w", "a"), ("c", "::"), ("w", "int"), ("p", "$1"), ("o", "<>"), ("w", "b")]
    assert order_digest("select a, v from t order by v desc", [("1", "9"), ("2", "9")]) == \
        order_digest("select a, v from t order by v desc", [("2", "9"), ("1", "9")]) != \
        order_digest("select a, v from t order by v desc", [("3", "1"), ("1", "9")])
    assert qfuzz_verdict(["1", "2"], ["2", "1"], False, "\x1f") == "ok" and qfuzz_verdict(["1", "2"], ["2", "1"], True, "\x1f") == "MISMATCH-ORDER"
    fb = float_smoke()
    for line in fb: print(line)
    bad += fb
    bad += regress_smoke()
    bad += send_smoke()
    print(f"multiset selftest: {'PASS' if not bad else 'FAIL'} ({len(files)} fixtures + the float smoke + the regress seam + the send seam, {len(bad)} failed)")
    return 1 if bad else 0


def main(a):
    if a[:1] == ["--selftest"]: return selftest()
    if a[:1] == ["classify"] and len(a) > 1:
        cls, keys = classify(a[1], a[2:] or None)
        tol = float_cols(a[1], a[2:] or None)
        print(cls + (" " + ",".join(map(str, keys)) if keys else "") + (" tol " + ",".join(f"{k}:{v}" for k, v in tol.items()) if tol else "")); return 0
    if a[:1] == ["case"]:
        res = [run_case(p) for p in a[1:]]
        for _, line in res: print(line)
        return 0 if all(ok for ok, _ in res) else 1
    print(__doc__); return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
