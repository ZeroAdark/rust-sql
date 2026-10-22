"""props/count.rs's mirror (road owner.count's shape, V4R-113): count_cut / chain / tie_key over graphprops.py's Walk,
split from graphprops.py for its file budget. Walk calls mark(), which adds the count_order prop."""
import os
import re
import sys

SELECT_MAX = 1024
MAX_REPS = 4
ORDERED_WORDS = {21, 23, 20, 16, 1082, 1083, 1114, 1184, 790}  # typesem::order_image == Signed (+ bool, never a word kind here)
# typesem int_arith by operator (pg_operator's int<widths><pl|mi|mul>): -> (op, left width, right width)
INT_ARITH = {o: (n[-2:] if n[-2:] in ("pl", "mi") else "mul", int(n[3]), int(n[4]) if n[4].isdigit() else int(n[3]))
             for o, n in {514: "int4mul", 526: "int2mul", 544: "int24mul", 545: "int42mul", 550: "int2pl", 551: "int4pl", 552: "int24pl",
                          553: "int42pl", 554: "int2mi", 555: "int4mi", 556: "int24mi", 557: "int42mi", 684: "int8pl", 685: "int8mi",
                          686: "int8mul", 688: "int84pl", 689: "int84mi", 690: "int84mul", 692: "int48pl", 693: "int48mi", 694: "int48mul",
                          818: "int82pl", 819: "int82mi", 820: "int82mul", 822: "int28pl", 823: "int28mi", 824: "int28mul"}.items()}
KEY_CHECK_KEYS, KEY_CHECK_STEPS = 4, 8
# NEW-46: keysem dependent_form's int casts by function (-> from width, to width) and int negations by operator (-> width)
INT_CAST = {313: (2, 4), 314: (4, 2), 480: (8, 4), 481: (4, 8), 714: (8, 2), 754: (2, 8)}
INT_NEG = {559: 2, 558: 4, 484: 8}
# owner_count.rs order_word: the slot kinds whose final the emit orders by its slot (F64Accum only for a float avg)
ORDERED_SLOTS = {"Count", "SumI64", "MinI64", "MaxI64", "SumI128", "SumNum", "SumNumWide", "AvgI128", "SumF64", "SumF32"}
# props/code.rs `raises`: the classes whose final cannot raise
NONRAISING = {"CountStar", "CountAny", "SumI64", "SumI128", "SumF32", "SumF64", "AvgI128", "Min", "Max", "BoolAnd", "BoolOr", "BitI64",
              "Collect"}


def G(w):
    """graphprops.py's module (the Walk's own: under __main__ too)."""
    return sys.modules[type(w).__module__]


def mark(w, p, group, inp, body):
    """Walk's hook: the count_order prop when a count cut sits over the group (props/count.rs count_cut)."""
    if group is not None and count_cut(w, p, group, inp, body):
        w.P.add("count_order")


def count_cut(w, p, group, inp, body):
    """props/code.rs `count_cut` (V4R-113): the top road owner.count's count form serves. A constant LIMIT (+ OFFSET) n in
    1..=SELECT_MAX without ties; the first key a final in a slot the emit orders (`order_word`, its sort operator taken as
    the type's default; a grouped DISTINCT's pair count or rider over its accumulator, `top_slot`); every later key
    names group key i in key order and passes `tie_key`; no grouping set; only projections before the sort, each (and the
    sort's list) of Vars, Consts, finals that cannot raise and integer chains over keys at one width (`chain`); a grouped
    count(DISTINCT) group by its pair count or a Count rider, no key check (D563)."""
    m = re.search(r" count=(\d+) offset=(\d+|all)(?:\s|$)", body)
    if not m or " ties" in body:
        return False
    n = int(m.group(1)) + (0 if m.group(2) == "all" else int(m.group(2)))
    sk = [(int(k.group(1)), k.group(2) == "-", bool(k.group(3)), int(k.group(4) or 0))
          for k in re.finditer(r"(\d+)([-+])(n?):\w+(?:/\w+)?(?:\.\d+)?(?:@(\d+))?", re.search(r"keys=(\S+)", body).group(1))]
    distinct = group["form"] == "Distinct" and w.grouped(group)  # D563: the pair owner's count form over its accumulator
    if not sk or not 1 <= n <= SELECT_MAX or group["set"] or len(sk) - 1 > len(group["keys"]) - distinct:
        return False
    agg = lambda e: w.agg_of(group, e) if e is not None and e.head == "agg" else None  # noqa: E731
    c = agg(w.entry(inp, sk[0][0]))
    if not c:
        return False
    # owner_count.rs top_slot: a grouped DISTINCT's pair count is the accumulator's Count, a DISTINCT fold's slot none
    pairs = distinct and c.get("final") == "PairCount"
    kind = "Count" if pairs else group["slots"][c["slot"]] if c["slot"] is not None and c["slot"] < len(group["slots"]) else None
    if c.get("final") in ("PairCount", "PairFold") and not pairs:
        kind = None
    if not (kind in ORDERED_SLOTS or (kind == "F64Accum" and c["cls"] == "AvgF64")):
        return False
    for i, ((resno, desc, nf, coll), k) in enumerate(zip(sk[1:], group["keys"])):
        e = w.entry(inp, resno)
        if e is None or e.head != "var" or e.extra != ("outer", k["resno"]):
            return False
        if not tie_key(w, group, group["keys"][:i], k, desc, nf, coll in (0, 950, 951), not distinct):
            return False

    checked = []

    def safe(lst):
        for t in lst or []:
            x = t.kids[0] if t.head == "te" else t
            while x.head == "relabel":
                x = x.kids[0]
            a = agg(x)
            if not (x.head in ("var", "const") or (a and a["cls"].split("(")[0] in NONRAISING)):
                k = chain(w, group, x)
                if k is None:
                    return False
                if k[1]:
                    checked.append(k)  # widening casts alone raise nothing
        return True
    before = []
    for name, b, _ in p.ops:
        if name == "sort":
            break
        before.append((name, b))
    ok = all(nm == "project" and safe(G(w).parse_list(b[len("project "):])) for nm, b in before)
    keys = {c[0] for c in checked}
    return (ok and (not distinct or not checked) and len({c[2] for c in checked}) <= 1 and len(keys) <= KEY_CHECK_KEYS
            and sum(c[1] for c in checked) <= KEY_CHECK_STEPS)


def chain(w, group, e):
    """count_cut.rs `chain`: integer + - * with a non-NULL constant (typesem int_arith), unary minus and int casts (NEW-46; a
    widening cast no step), nested, over one int group key (`key_of`), every raising step at one width: (key index, steps,
    width), or None."""
    v = value(w, group, e)
    return None if v is None else (v[0], v[1], v[3])


def value(w, group, e):
    """`chain`'s walk: (key index, steps, value width, raise width (0: no step)), or None."""
    def bare(x):
        while x.head == "relabel":
            x = x.kids[0]
        return x
    one = lambda r, wd: wd if r in (0, wd) else None
    if e.head == "var":
        k = key_index(w, group, e)
        return None if k is None else (k[0], 0, k[1], 0)
    if e.head == "fn" and e.oid in INT_CAST and len(e.kids) == 1:
        inner = value(w, group, bare(e.kids[0]))
        frm, to = INT_CAST[e.oid]
        if inner is None or inner[2] != frm:
            return None
        if to > frm:
            return (inner[0], inner[1], to, inner[3])
        r = one(inner[3], to)
        return None if r is None else (inner[0], inner[1] + 1, to, r)
    if e.head == "op" and e.oid in INT_NEG and len(e.kids) == 1:
        inner, wd = value(w, group, bare(e.kids[0])), INT_NEG[e.oid]
        r = None if inner is None or inner[2] != wd else one(inner[3], wd)
        return None if r is None else (inner[0], inner[1] + 1, wd, r)
    a = INT_ARITH.get(e.oid) if e.head == "op" and len(e.kids) == 2 else None
    if a is None:
        return None
    kids = [bare(x) for x in e.kids]
    result = max(a[1], a[2])
    side = 0 if kids[1].head == "const" and kids[1].text != "null" else 1 if kids[0].head == "const" and kids[0].text != "null" else None
    if side is None:
        return None
    inner, vw = value(w, group, kids[side]), a[1 + side]
    r = None if inner is None or inner[2] != vw else one(inner[3], result)
    return None if r is None else (inner[0], inner[1] + 1, result, r)


def key_index(w, group, v):
    """count_cut.rs `key_of`: the group key outer Var `v` names, an int key read raw (a scan column, or NEW-46b any key the
    producer computes), and its width; a nullable one (NEW-46) where every key before it is one word (its record word is its
    index). Nullable here is anything but a NOT NULL scan column (emit_group.rs `key_nullable` also calls a dense range form
    of a NOT NULL column not nullable: this mirror asks that one for the one-word rule too, a conservative miss)."""
    keys = [i for i, k in enumerate(group["keys"]) if v.head == "var" and v.extra[0] == "outer" and k["resno"] == v.extra[1]]
    if len(keys) != 1:
        return None
    k = group["keys"][keys[0]]
    width = {21: 2, 23: 4, 20: 8}.get(k["type"])
    if width is None or k["kind"] != ("I64" if width == 8 else "I32") or k["image"] not in (None, "Raw"):
        return None
    prod = w.g.pipes[group["producer"]]
    m = re.search(r"\brel=(\d+) rti=(\d+)", prod.source or "")
    v = k["expr"]
    if not m:
        return None  # count_cut.rs `scanned`: a breaker-fed producer declines
    scan = v.head == "var" and v.extra[0] == "v" + m.group(2)
    if not (scan and att_notnull(w, w.g.fid, int(m.group(1)), v.extra[1])) and k.get("word") != keys[0]:
        return None
    return keys[0], width


def tie_key(w, group, before, k, desc, nulls_first, bytes_order, flags):
    """count_cut.rs `tie_key`: the owner orders the named key k as the SQL does (a word ascending, a scan column or NEW-46b
    one the producer computes, NULLS LAST by its flag where nullable and `flags`; or a C-ordered text scan column ascending
    by its representative over a dictionary code lane, NULLS LAST where nullable). Nullable is anything but a NOT NULL scan
    column (`key_index`'s conservative reading)."""
    prod = w.g.pipes[group["producer"]]
    m = re.search(r"\brel=(\d+) rti=(\d+)", prod.source or "")
    v = k["expr"]
    if desc:
        return False
    scan = bool(m) and v.head == "var" and v.extra[0] == "v" + m.group(2)
    nn = scan and att_notnull(w, w.g.fid, int(m.group(1)), v.extra[1])
    if k["kind"] in ("I32", "I64", "Date", "Timestamp") and k["image"] in (None, "Raw") and k["type"] in ORDERED_WORDS:
        return bool(m) and (nn or (flags and not nulls_first))  # count_cut.rs `scanned`: a breaker-fed producer declines
    if not scan:
        return False
    att = v.extra[1]
    text = sum(1 for b in before if b["kind"] == "Fp128")
    if (k["kind"] != "Fp128" or k["image"] not in (None, "Text") or k["type"] not in (25, 1043) or not bytes_order
            or k["rep"] is None or text >= MAX_REPS or (nulls_first and not nn)):
        return False
    for ln in prod.src_lines:
        if ln.startswith("lanes"):
            for t in ln.split()[1:]:
                lm = re.match(r"!?Attr\((\d+)\):[a-z]+\d*:([^:]+):(\S*)", t)
                if lm and int(lm.group(1)) == att:
                    return G(w).lane_family(lm.group(2)) == "code" and "dict" in lm.group(3)
    return False


def att_notnull(w, fid, relid, att):
    """props `facts.att(relid, att).notnull` from the plan fixture's `-- att <relid> <attno> <name> ... notnull=t|f` line."""
    p = G(w).plan_path(fid) if fid else None
    if not p or not os.path.exists(p):
        return False
    with open(p, errors="replace") as fh:
        for line in fh:
            if not line.startswith("-- "):
                break
            m = re.match(r"^-- att %d %d \S+ .*\bnotnull=(\w)" % (relid, att), line)
            if m:
                return m.group(1) == "t"
    return False


def key_extra(k, m):
    """graphprops.py's key line: the scan attribute and the representative slot owner.count's tie keys read."""
    rp = re.search(r"\brep=(\d+)", m.group(5))
    wd = re.match(r"key \d+ word=(\d+)", m.string)
    k.update(attno=int(m.group(3)), rep=int(rp.group(1)) if rp else None, word=int(wd.group(1)) if wd else None)


def agg_extra(a, body):
    """graphprops.py's agg line: the record slot and the final form owner.count's count cut reads."""
    sl, fi = re.search(r" slot=(\d+) ", body), re.search(r" final=(\S+)", body)
    a.update(slot=int(sl.group(1)) if sl else None, final=fi.group(1) if fi else None)
