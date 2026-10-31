#!/usr/bin/env python3
"""SQL order classification shared by regression and differential tooling.

Extracted without semantic changes from multiset.py so classification does not
require optional comparison/normalization modules absent from the source tree.
"""
import re

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
