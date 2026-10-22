#!/usr/bin/env python3
"""Print each listed type's catalog facts from PostgreSQL 18.6's .dat files: typlen/byval/align/collation, the default btree
opclass's `=` proc and support-1 cmp proc, the default hash opclass's support-1 proc, and typinput/typoutput procs.
Usage: typefacts_procs.py <catalog dir> <typid>...  (the rows of src/catalog/typefacts.rs were seeded from its output)."""
import re, sys
D = sys.argv[1]
def load(name):
    txt = open(f"{D}/{name}.dat").read()
    out = []
    for m in re.finditer(r"\{(.*?)\}", txt, re.S):
        d = dict(re.findall(r"(\w+)\s*=>\s*'([^']*)'", m.group(1)))
        if d:
            out.append(d)
    return out
types, procs, opcl, opf, amop, amproc, oper = (load(n) for n in ["pg_type", "pg_proc", "pg_opclass", "pg_opfamily", "pg_amop", "pg_amproc", "pg_operator"])
toid = {t["oid"]: t for t in types}
pbyname = {}
for p in procs:
    pbyname.setdefault(p["proname"], []).append(p)
def proc_oid(name, argtypes=None):
    if name.isdigit():
        return int(name)
    if "(" in name:
        name = name.split("(")[0]
    c = pbyname.get(name, [])
    if argtypes is not None:
        c2 = [p for p in c if p.get("proargtypes", "").split() == argtypes]
        if c2:
            c = c2
    return int(c[0]["oid"]) if c else 0
opbykey = {}
for o in oper:
    opbykey[(o["oprname"], o.get("oprleft"), o.get("oprright"))] = o
PARENT = {"varchar": "text", "cidr": "inet", "json": None, "regproc": "oid", "regprocedure": "oid", "regoper": "oid", "regoperator": "oid",
          "regclass": "oid", "regtype": "oid", "regconfig": "oid", "regdictionary": "oid", "regnamespace": "oid", "regrole": "oid",
          "regcollation": "oid"}
def default_opclass(method, tname):
    c = default_opclass1(method, tname)
    if c is None:
        cat = next((t.get("typcategory") for t in types if t.get("typname") == tname), None)
        parent = "anymultirange" if tname.endswith("multirange") else "anyrange" if cat == "R" else "anyarray" if cat == "A" else PARENT.get(tname)
        if parent:
            c = default_opclass1(method, parent)
    return c
def default_opclass1(method, tname):
    for c in opcl:
        if c.get("opcmethod") == method and c.get("opcintype") == tname and c.get("opcdefault", "t") == "t":
            return c
    return None
for typid in sys.argv[2:]:
    t = toid.get(typid)
    if t is None:
        e = next(x for x in types if x.get("array_type_oid") == typid)
        t = {"oid": typid, "typname": "_" + e["typname"], "typlen": "-1", "typbyval": "f", "typalign": "d" if e.get("typalign") == "d" else "i", "typcategory": "A", "typinput": "array_in", "typoutput": "array_out", "typcollation": e.get("typcollation", "")}
        types.append(t)
    tn = t["typname"]
    bt = default_opclass("btree", tn)
    h = default_opclass("hash", tn)
    eq = cmp = hs = 0
    if bt:
        fam = bt["opcfamily"].split("/")[1]
        it = bt["opcintype"]
        for a in amop:
            if a["amopfamily"] == f"btree/{fam}" and a["amopstrategy"] == "3" and a["amoplefttype"] == it and a["amoprighttype"] == it:
                opn = a["amopopr"]
                m = re.match(r"(.*)\((.*),(.*)\)", opn)
                o = opbykey.get((m.group(1), m.group(2), m.group(3)))
                if o:
                    eq = proc_oid(o["oprcode"])
        for a in amproc:
            if a["amprocfamily"] == f"btree/{fam}" and a["amprocnum"] == "1" and a["amproclefttype"] == it and a["amprocrighttype"] == it:
                cmp = proc_oid(a["amproc"])
    if h:
        fam = h["opcfamily"].split("/")[1]
        it = h["opcintype"]
        for a in amproc:
            if a["amprocfamily"] == f"hash/{fam}" and a["amprocnum"] == "1" and a["amproclefttype"] == it and a["amprocrighttype"] == it:
                hs = proc_oid(a["amproc"])
    typlen = {"NAMEDATALEN": "64", "SIZEOF_POINTER": "8"}.get(t["typlen"], t["typlen"])
    coll = t.get("typcollation", "") == "default" or t.get("typcollation", "") == "C"
    print(f"{typid} {tn} len={typlen} byval={t.get('typbyval','f')} align={t.get('typalign','i')} coll={int(coll)} "
          f"eq={eq} cmp={cmp} hash={hs} in={proc_oid(t['typinput'])} out={proc_oid(t['typoutput'])} cat={t.get('typcategory')}")
