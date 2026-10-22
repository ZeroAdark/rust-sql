#!/usr/bin/env python3
"""FACTS-1b (M8-plan §7.1): write src/effects/procfacts.rs, the builtin catalog's function and operator properties decompose's facts
read (`fnprops.rs`), from PostgreSQL 18.6's .dat files under crates/postgres-18.6-reference/src/include/catalog:
  - per pg_proc row: provolatile, proisstrict, proleakproof, proparallel;
  - per pg_operator row: oprcode, oprcom (the commutator, 0 = none), oprnegate (the negator, 0 = none), oprcanhash, oprcanmerge and the first hash opfamily holding it as strategy 1 `=` (pg_amop,
    the smallest opfamily oid: the AMOPOPID order `get_op_hash_functions` walks);
  - per btree pg_amop row: the operator, its opfamily, left and right input types and strategy (normalize's same-type
    comparison of a cross-type one: `btree_op`);
  - per default btree pg_opclass row: its input type and opfamily (the type cache's `btree_opf`: catalog opdir.rs's
    bootstrap answers, OIDW-81 / OIDW-82);
  - per pg_proc row whose C function is a LIKE / ILIKE / regular expression match (its `prosrc`: textlike, namelike,
    texticregexeq, ...; bpchar's and the like() aliases call the same ones): the match's form (the effects home's
    constant-pattern rule, DC-14);
  - per pg_proc row of the SQL `extract(field FROM x)` function (proname `extract`, a numeric result): the class of its
    source `x` (date, time, timetz, timestamp, timestamptz, interval), whose field ranges decide an integral field's key
    word (agg keys.rs `integral_extract`, DC-09 / OID-06);
  - per pg_type row (and the array type genbki makes for it): typinput and typoutput (a CoerceViaIO's calls), and the
    get_promoted_array_type of the type (an ARRAY sublink's result type, D1045).
A field a .dat row omits takes its column's BKI_DEFAULT (pg_proc.h: provolatile 'i', proisstrict 't', proleakproof 'f', proparallel 's';
pg_operator.h: oprcanhash / oprcanmerge 'f'; main 07:21Z): `DEFAULTS` below, the one place they are read.
Usage: procfacts.py            rewrite src/effects/procfacts.rs
       procfacts.py --check    exit 1 (naming the file) when src/effects/procfacts.rs differs from what it would write
       procfacts.py --selftest the BKI defaults over synthetic rows that omit each field"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
CRATE = os.path.dirname(HERE)
CATDIR = os.path.join(CRATE, "../../postgres-18.6-reference/src/include/catalog")
OUT = os.path.join(CRATE, "src/effects/procfacts.rs")

DEFAULTS = {"provolatile": "i", "proisstrict": "t", "proleakproof": "f", "proparallel": "s", "oprcanhash": "f", "oprcanmerge": "f"}
VOL = {"i": 0, "s": 1, "v": 2}
# D1035 step 2 (V4R-444): pg_proc.proparallel, the planner's parallel hazard (safe, restricted, unsafe)
PAR = {"s": 0, "r": 1, "u": 2}
# an operator over these input types hashes per the actual type (array_eq / record_eq / range_eq: `op_hashjoinable` and
# hash_array / hash_range may find no hash function); anyenum hashes every enum (hashenum)
POLY = {"anyarray", "anyelement", "anynonarray", "anyrange", "anymultirange", "record", "anycompatible", "anycompatiblearray",
        "anycompatiblenonarray", "anycompatiblerange", "anycompatiblemultirange", "internal", "any"}


def parse(text):
    """A .dat file's rows as dicts: quoted values (backslash escapes), `#` comment lines skipped (genbki's Catalog.pm form)."""
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
            elif c == ",":
                key = []
        elif c == "#":
            while i < len(text) and text[i] != "\n":
                i += 1
        elif c == "{":
            cur, key = {}, []
        i += 1
    return out


def dat(name):
    with open(os.path.join(CATDIR, name)) as f:
        return parse(f.read())


def field(row, k):
    return row.get(k, DEFAULTS[k])


def proc_flags(row):
    """bits 0-1 volatility (0 immutable, 1 stable, 2 volatile), bit 2 strict, bit 3 leakproof, bits 4-5 proparallel (0 s, 1 r, 2 u),
    bit 6 a LANGUAGE sql body (prolang 'sql': initdb's `select $2 + $1` commutators and the like, D3577)"""
    return VOL[field(row, "provolatile")] | (4 if field(row, "proisstrict") == "t" else 0) | (8 if field(row, "proleakproof") == "t" else 0) | PAR[field(row, "proparallel")] << 4 | (64 if row.get("prolang", "internal") == "sql" else 0)


def tables(procs, opers, amops, fams, types, opcs):
    names = {}
    rows = []
    for p in procs:
        if "oid" not in p:
            continue
        names.setdefault(p["proname"], int(p["oid"]))
        rows.append((int(p["oid"]), proc_flags(p)))
    fam = {}
    for f in fams:
        fam["%s/%s" % (f["opfmethod"], f["opfname"])] = int(f["oid"])
    opsig = {}
    for o in opers:
        if "oid" in o:
            opsig["%s(%s,%s)" % (o["oprname"], o.get("oprleft", "0"), o.get("oprright", "0"))] = int(o["oid"])
    hashfam = {}
    for a in amops:
        if a.get("amopmethod") != "hash" or a.get("amopstrategy") != "1":
            continue
        op = opsig[a["amopopr"]]
        f = fam[a["amopfamily"]]
        hashfam[op] = min(hashfam.get(op, f), f)
    orows = []
    for o in opers:
        if "oid" not in o:
            continue
        code = o.get("oprcode", "-")
        code = int(code) if code.isdigit() else names.get(code, 0)
        flags = (1 if field(o, "oprcanhash") == "t" else 0) | (2 if field(o, "oprcanmerge") == "t" else 0)
        flags |= 4 if o.get("oprleft") in POLY or o.get("oprright") in POLY else 0
        com = opsig[o["oprcom"]] if "oprcom" in o else 0
        neg = opsig[o["oprnegate"]] if "oprnegate" in o else 0
        orows.append((int(o["oid"]), code, hashfam.get(int(o["oid"]), 0), flags, com, neg))
    trows = []
    for t in types:
        if "oid" not in t:
            continue
        trows.append((int(t["oid"]), names[t["typinput"]], names[t["typoutput"]]))
        if "array_type_oid" in t:
            trows.append((int(t["array_type_oid"]), names["array_in"], names["array_out"]))
    return sorted(rows), sorted(orows), sorted(trows), promoted(types), btree(amops, fam, opsig, types), default_btree(opcs, fam, types), patterns(procs), extracts(procs)


def btree(amops, fam, opsig, types):
    """`(operator, opfamily, lefttype, righttype, strategy)` of each btree pg_amop row, sorted."""
    oid = {t["typname"]: int(t["oid"]) for t in types if "oid" in t}
    return sorted((opsig[a["amopopr"]], fam[a["amopfamily"]], oid[a["amoplefttype"]], oid[a["amoprighttype"]], int(a["amopstrategy"]))
                  for a in amops if a.get("amopmethod") == "btree")


def default_btree(opcs, fam, types):
    """`(opcintype, opfamily)` of each default btree pg_opclass row (opcdefault, BKI default 't'), sorted: GetDefaultOpClass's
    family for the type, the type cache's `btree_opf`."""
    oid = {t["typname"]: int(t["oid"]) for t in types if "oid" in t}
    return sorted((oid[c["opcintype"]], fam[c["opcfamily"]]) for c in opcs if c.get("opcmethod") == "btree" and c.get("opcdefault", "t") == "t")


# a pattern match's C function (pg_proc.prosrc) -> its form: 1 LIKE / NOT LIKE, 2 ILIKE / NOT ILIKE, 3 `~` / `!~`, 4 `~*` / `!~*`
PATTERN_SRC = {"textlike": 1, "textnlike": 1, "namelike": 1, "namenlike": 1,
               "texticlike": 2, "texticnlike": 2, "nameiclike": 2, "nameicnlike": 2,
               "textregexeq": 3, "textregexne": 3, "nameregexeq": 3, "nameregexne": 3,
               "texticregexeq": 4, "texticregexne": 4, "nameicregexeq": 4, "nameicregexne": 4}


def patterns(procs):
    """`(pg_proc oid, form)` of each builtin whose C function is a pattern match (`PATTERN_SRC`), sorted."""
    return sorted((int(p["oid"]), PATTERN_SRC[p.get("prosrc", p["proname"])]) for p in procs
                  if "oid" in p and p.get("prolang", "internal") == "internal" and p.get("prosrc", p["proname"]) in PATTERN_SRC)


# an `extract` source type's class: 1 date, 2 time, 3 timetz, 4 timestamp, 5 timestamptz, 6 interval (effects
# `ExtractSource`); a source outside it is a new catalog row to classify, so the generator stops on it
EXTRACT_SRC = {"date": 1, "time": 2, "timetz": 3, "timestamp": 4, "timestamptz": 5, "interval": 6}


def extracts(procs):
    """`(pg_proc oid, source class)` of each builtin SQL `extract(field FROM x)` (proname `extract`, internal, a numeric
    result, arguments (text, x)), sorted; x's class from `EXTRACT_SRC`."""
    out = []
    for p in procs:
        if "oid" not in p or p["proname"] != "extract" or p.get("prolang", "internal") != "internal" or p.get("prorettype") != "numeric":
            continue
        args = p.get("proargtypes", "").split()
        assert len(args) == 2 and args[0] == "text" and args[1] in EXTRACT_SRC, "extract source to classify: %r" % p
        out.append((int(p["oid"]), EXTRACT_SRC[args[1]]))
    return sorted(out)


def promoted(types):
    """`get_promoted_array_type` per builtin type (an ARRAY sublink's result type, D1045): the type's array type (genbki's
    `array_type_oid`, or an explicit `typarray`), else the type itself when it is a true array (`typelem` with
    array_subscript_handler, or an array genbki makes), else none (no row)."""
    oid = {t["typname"]: int(t["oid"]) for t in types if "oid" in t}
    out = {}
    for t in types:
        if "oid" not in t:
            continue
        o = int(t["oid"])
        if "array_type_oid" in t:
            a = int(t["array_type_oid"])
            out[o] = a
            out[a] = a  # the made array: no array type of its own, its element valid
        elif "typarray" in t:
            out[o] = oid[t["typarray"]]
        elif t.get("typsubscript") == "array_subscript_handler" and "typelem" in t:
            out.setdefault(o, o)
    return sorted(out.items())


def chunks(items, n):
    for i in range(0, len(items), n):
        yield items[i:i + n]


def render(rows, orows, trows, arows, brows, drows, prows, xrows):
    assert all(o < 1 << 16 for o, _ in rows) and all(max(r[:3] + r[4:]) < 1 << 16 for r in orows) and all(max(r) < 1 << 16 for r in brows + drows)
    out = ["// @generated by crates/gravity/decompose/gen/procfacts.py from PostgreSQL 18.6's pg_proc / pg_operator / pg_amop /",
           "// pg_opfamily / pg_opclass .dat files (BKI defaults applied): do not edit; rerun the generator (tests/facts_m8b.rs runs --check).",
           "",
           "/// `(pg_proc oid, flags)`, sorted: bits 0-1 provolatile (0 i, 1 s, 2 v), bit 2 proisstrict, bit 3 proleakproof, bits 4-5 proparallel (0 s, 1 r, 2 u), bit 6 prolang sql.",
           "pub(crate) static PROCS: &[(u16, u8)] = &["]
    for c in chunks(rows, 12):
        out.append("    " + " ".join("(%d, %d)," % r for r in c))
    out.append("];")
    out.append("")
    out.append("/// `(pg_operator oid, oprcode, first hash opfamily with it as `=` or 0, flags, oprcom or 0, oprnegate or 0)`, sorted: bit 0 oprcanhash, bit 1 oprcanmerge, bit 2 a polymorphic / record input.")
    out.append("pub(crate) static OPERS: &[(u16, u16, u16, u8, u16, u16)] = &[")
    for c in chunks(orows, 8):
        out.append("    " + " ".join("(%d, %d, %d, %d, %d, %d)," % r for r in c))
    out.append("];")
    out.append("")
    out.append("/// `(pg_type oid, typinput, typoutput)`, sorted; an array type genbki makes (`array_type_oid`) reads through array_in / array_out.")
    out.append("pub(crate) static TYPES: &[(u16, u16, u16)] = &[")
    for c in chunks(trows, 8):
        out.append("    " + " ".join("(%d, %d, %d)," % r for r in c))
    out.append("];")
    out.append("")
    out.append("/// `(pg_type oid, get_promoted_array_type)`, sorted: the result type of an ARRAY sublink over the type.")
    out.append("pub(crate) static PROMOTED: &[(u16, u16)] = &[")
    for c in chunks(arows, 12):
        out.append("    " + " ".join("(%d, %d)," % r for r in c))
    out.append("];")
    out.append("")
    out.append("/// `(pg_operator oid, btree opfamily, lefttype, righttype, strategy)` of each btree pg_amop row, sorted.")
    out.append("pub(crate) static BTREE: &[(u16, u16, u16, u16, u8)] = &[")
    for c in chunks(brows, 8):
        out.append("    " + " ".join("(%d, %d, %d, %d, %d)," % r for r in c))
    out.append("];")
    out.append("")
    out.append("/// `(opcintype, opfamily)` of each default btree pg_opclass row, sorted: the type's default btree family (`btree_opf`).")
    out.append("pub(crate) static BTREE_DEFAULT: &[(u16, u16)] = &[")
    for c in chunks(drows, 12):
        out.append("    " + " ".join("(%d, %d)," % r for r in c))
    out.append("];")
    out.append("")
    out.append("/// `(pg_proc oid, form)` of each builtin pattern match, sorted: 1 LIKE / NOT LIKE, 2 ILIKE / NOT ILIKE, 3 `~` / `!~`, 4 `~*` / `!~*`.")
    out.append("pub(crate) static PATTERNS: &[(u16, u8)] = &[")
    for c in chunks(prows, 12):
        out.append("    " + " ".join("(%d, %d)," % r for r in c))
    out.append("];")
    out.append("")
    out.append("/// `(pg_proc oid, source class)` of each builtin SQL `extract(field FROM x)`, sorted: x is 1 date, 2 time, 3 timetz, 4 timestamp, 5 timestamptz, 6 interval.")
    out.append("pub(crate) static EXTRACTS: &[(u16, u8)] = &[")
    for c in chunks(xrows, 12):
        out.append("    " + " ".join("(%d, %d)," % r for r in c))
    out.append("];")
    return "\n".join(out) + "\n"


def generate():
    return render(*tables(dat("pg_proc.dat"), dat("pg_operator.dat"), dat("pg_amop.dat"), dat("pg_opfamily.dat"), dat("pg_type.dat"), dat("pg_opclass.dat")))


def selftest():
    rows = parse("""[
{ oid => '1', proname => 'bare', prorettype => 'int4', proargtypes => 'int4' },
{ oid => '2', proname => 'vol', provolatile => 'v', proisstrict => 'f', proleakproof => 't' },
{ oid => '3', proname => 'stab', provolatile => 's' },
# a comment { oid => '9' }
{ oid => '4', proname => 'q\\'uote', proleakproof => 't' },
{ oid => '5', proname => 'restr', proparallel => 'r' },
{ oid => '6', proname => 'uns', provolatile => 's', proparallel => 'u' },
{ oid => '7', proname => 'sqlbody', prolang => 'sql', prosrc => 'select $2 + $1' },
]""")
    got = [(int(r["oid"]), proc_flags(r)) for r in rows]
    assert got == [(1, 4), (2, 2 | 8), (3, 1 | 4), (4, 4 | 8), (5, 4 | 16), (6, 1 | 4 | 32), (7, 4 | 64)], got
    ops = parse("""[
{ oid => '10', oprname => '=', oprleft => 'int4', oprright => 'int4', oprcode => 'bare', oprnegate => '<>(int4,int4)' },
{ oid => '15', oprname => '<>', oprleft => 'int4', oprright => 'int4', oprcode => 'bare', oprnegate => '=(int4,int4)' },
{ oid => '11', oprname => '=', oprleft => 'int8', oprright => 'int8', oprcode => 'vol', oprcanhash => 't', oprcanmerge => 't', oprcom => '=(int8,int8)' },
{ oid => '13', oprname => '<', oprleft => 'int4', oprright => 'int8', oprcode => 'bare', oprcom => '>(int8,int4)' },
{ oid => '14', oprname => '>', oprleft => 'int8', oprright => 'int4', oprcode => 'bare', oprcom => '<(int4,int8)' },
{ oid => '12', oprname => '=', oprleft => 'anyarray', oprright => 'anyarray', oprcode => 'bare', oprcanhash => 't' },
]""")
    amops = parse("""[
{ amopfamily => 'hash/b', amoplefttype => 'int8', amoprighttype => 'int8', amopstrategy => '1', amopopr => '=(int8,int8)', amopmethod => 'hash' },
{ amopfamily => 'hash/a', amoplefttype => 'int8', amoprighttype => 'int8', amopstrategy => '1', amopopr => '=(int8,int8)', amopmethod => 'hash' },
{ amopfamily => 'btree/c', amoplefttype => 'int4', amoprighttype => 'int4', amopstrategy => '3', amopopr => '=(int4,int4)', amopmethod => 'btree' },
]""")
    fams = parse("{ oid => '20', opfmethod => 'hash', opfname => 'b' }, { oid => '19', opfmethod => 'hash', opfname => 'a' }, { oid => '21', opfmethod => 'btree', opfname => 'c' }")
    tys = parse("{ oid => '23', typname => 'int4', typinput => 'bare', typoutput => 'bare' }")
    opcs = parse("""[
{ opcmethod => 'btree', opcname => 'c_ops', opcfamily => 'btree/c', opcintype => 'int4' },
{ opcmethod => 'btree', opcname => 'c_alt_ops', opcfamily => 'btree/c', opcintype => 'int4', opcdefault => 'f' },
{ opcmethod => 'hash', opcname => 'b_ops', opcfamily => 'hash/b', opcintype => 'int4' },
]""")
    _, orows, _, _, brows, drows, prows, xrows = tables(rows, ops, amops, fams, tys, opcs)
    assert brows == [(10, 21, 23, 23, 3)], brows
    assert drows == [(23, 21)], drows
    assert prows == [], prows
    pats = parse("""[
{ oid => '30', proname => 'namelike', prosrc => 'namelike' },
{ oid => '31', proname => 'like', prosrc => 'textlike' },
{ oid => '32', proname => 'bpcharicregexne', prosrc => 'texticregexne' },
{ oid => '33', proname => 'textlike', prolang => 'sql', prosrc => 'select 1' },
{ oid => '34', proname => 'bytealike' },
]""")
    assert patterns(pats) == [(30, 1), (31, 1), (32, 4)], patterns(pats)
    assert xrows == [], xrows
    xs = parse("""[
{ oid => '40', proname => 'extract', prorettype => 'numeric', proargtypes => 'text interval', prosrc => 'extract_interval' },
{ oid => '41', proname => 'date_part', prorettype => 'float8', proargtypes => 'text interval', prosrc => 'interval_part' },
{ oid => '42', proname => 'extract', prolang => 'sql', prorettype => 'numeric', proargtypes => 'text date', prosrc => 'select 1' },
{ oid => '43', proname => 'extract', provolatile => 's', prorettype => 'numeric', proargtypes => 'text timestamptz' },
]""")
    assert extracts(xs) == [(40, 6), (43, 5)], extracts(xs)
    assert orows == [(10, 1, 0, 0, 0, 15), (11, 2, 19, 3, 11, 0), (12, 1, 0, 5, 0, 0), (13, 1, 0, 0, 14, 0), (14, 1, 0, 0, 13, 0), (15, 1, 0, 0, 0, 10)], orows
    print("procfacts selftest: ok")


if __name__ == "__main__":
    if "--selftest" in sys.argv:
        selftest()
    elif "--check" in sys.argv:
        with open(OUT) as f:
            have = f.read()
        if have != generate():
            print("STALE %s: rerun crates/gravity/decompose/gen/procfacts.py" % os.path.relpath(OUT, CRATE), file=sys.stderr)
            sys.exit(1)
    else:
        with open(OUT, "w") as f:
            f.write(generate())
