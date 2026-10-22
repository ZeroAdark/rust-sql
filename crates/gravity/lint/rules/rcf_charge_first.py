"""RCF: a memledger Charge declared AFTER the heap it counts (memlead 2026-10-15T21:30:28Z, via memacct; D4537). Rust drops
a struct's fields in declaration order. A struct holding a `Charge` (memledger's owned charge) or memledger's `Held` account hold DIRECTLY,
beside heap-owning fields, must declare
the charge-holding field FIRST: a Charge dropped after the bytes it counts leaves the thread's heap below its charges for
the span between (a lone-thread drop dips below zero, and ledger_floor floors it, hiding the gap).

RCF reads every production .rs file under crates/ (tests, `#[cfg(test)]` modules, the C reference and the lint left out)
and finds each struct (named or tuple fields) that declares a heap-owning field (a std heap type named in its type: Vec,
Box, String, HashMap / HashSet / BTreeMap / BTreeSet / VecDeque / BinaryHeap, Rc, Arc, CString / OsString / PathBuf) before
a charge-holding field. A site not reordered yet needs a row in lint/CHARGE-ORDER-DEBT (a directory, one row per file):
path, struct, owner lane and why it stays; an unlisted one FAILS. On a landing (--base) a row the parent lacks fails (the
list only shrinks), unless the parent has no ledger yet. A row whose struct is in order (or gone) is a note (delete it).
By name, not by type: `Charge` anywhere; `Held` where the file imports memledger's (or names memledger::Held). Only a
DIRECT field counts: a field whose type merely holds a charge of its own (ChargedVec, RegionBuf, an OpenPart, any struct
with its own Charge) charges its own contents, freed and uncharged together in its own drop, so its place among the outer
struct's fields moves no byte (alloclint 2026-10-15T22:3xZ: the one-level holders were 23 of the first 44 hits and a
false hit on trunk's DecodeShared, whose ChargedVecs count only their own buffers)."""
import os
import re

from common import G, ledger_loc, Src, generated, in_any

ID = "RCF"
TITLE = "a Charge declared after the heap it counts"
INSTEAD = "declare the Charge (or the field that owns one) FIRST: fields drop in declaration order, so the charge goes " \
          "before the bytes it counts; not yet reordered: a lint/CHARGE-ORDER-DEBT row"
GLOBAL = True
DEBT = G + "lint/CHARGE-ORDER-DEBT"
ROOTS = ("crates/",)
SKIP = ("crates/postgres-18.6-reference/", "crates/context/", G + "lint/")

STRUCT = re.compile(r"\bstruct\s+([A-Za-z_]\w*)\s*(<)?")
HEAP = re.compile(r"(?<![\w:])(?:(?:std|alloc)::(?:vec|boxed|string|collections|rc|sync|ffi|path)::(?:\w+::)?)?"
                  r"(Vec|Box|String|HashMap|HashSet|BTreeMap|BTreeSet|VecDeque|BinaryHeap|Rc|Arc|CString|OsString|PathBuf)\b")
BASE_HOLDERS = {"Charge"}
ML_HELD = re.compile(r"\bmemledger::(?:\{[^}]*\bHeld\b[^}]*\}|Held\b)")
CFG_TEST_MOD = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{")


def is_test_path(p):
    base = os.path.basename(p)
    return ("/tests/" in p or "/benches/" in p or "/examples/" in p or base in ("tests.rs", "test.rs")
            or base.endswith("_tests.rs") or base.endswith("_test.rs"))


def sources(ctx):
    out = []
    for r in ROOTS:
        top = os.path.join(ctx.root, r)
        for dp, dn, fn in os.walk(top):
            dn[:] = sorted(d for d in dn if d not in ("target", "tests", "benches", "examples") and not d.startswith("."))
            for f in sorted(fn):
                if f.endswith(".rs"):
                    p = os.path.relpath(os.path.join(dp, f), ctx.root).replace(os.sep, "/")
                    if not in_any(p, list(SKIP)) and not is_test_path(p):
                        out.append(p)
    return out


def _close(t, i, o, c):
    depth = 0
    for k in range(i, len(t)):
        if t[k] == o:
            depth += 1
        elif t[k] == c:
            depth -= 1
            if depth == 0:
                return k
    return -1


def _split_top(body):
    """the body's fields, split at top-level commas (generics, tuples, arrays and fn types nest)"""
    out, depth, cur = [], 0, []
    prev = ""
    for ch in body:
        if ch in "<([{":
            depth += 1
        elif ch in ")]}" or (ch == ">" and prev != "-"):
            depth -= 1
        if ch == "," and depth == 0:
            out.append("".join(cur))
            cur = []
        else:
            cur.append(ch)
        prev = ch
    out.append("".join(cur))
    return [x for x in out if x.strip()]


def _field_type(f, named):
    s = re.sub(r"#\s*\[[^\]]*\]", " ", f).strip()
    s = re.sub(r"^pub(?:\s*\([^)]*\))?\s+", "", s)
    if named:
        return s.split(":", 1)[1].strip() if ":" in s else ""
    return s


def structs(src):
    """-> [(name, line, [field type])] of src's structs outside #[cfg(test)] modules (memledger's Held, where the file imports
    it, reads as `Charge`)"""
    t = src.joined("code")
    held = bool(ML_HELD.search(t)) or src.path.startswith("crates/_support/memledger/")
    tests = []
    for m in CFG_TEST_MOD.finditer(t):
        e = _close(t, m.end() - 1, "{", "}")
        tests.append((m.start(), e if e >= 0 else len(t)))
    out = []
    for m in STRUCT.finditer(t):
        if any(a <= m.start() < b for a, b in tests):
            continue
        k = m.end()
        if m.group(2):   # generics
            depth, k = 1, m.end()
            while k < len(t) and depth:
                if t[k] == "<":
                    depth += 1
                elif t[k] == ">" and t[k - 1] != "-":
                    depth -= 1
                k += 1
        rest = t[k:k + 4000]
        ob = re.match(r"\s*(?:where\b[^{;(]*)?([{(;])", rest)
        if not ob or ob.group(1) == ";":
            continue
        i = k + ob.end() - 1
        named = ob.group(1) == "{"
        e = _close(t, i, "{" if named else "(", "}" if named else ")")
        if e < 0:
            continue
        fields = [_field_type(f, named) for f in _split_top(t[i + 1:e])]
        if held:
            fields = [re.sub(r"(?:memledger::)?\bHeld\b", "Charge", f) for f in fields]
        out.append((m.group(1), t.count("\n", 0, m.start()) + 1, fields))
    return out


def _words(ty):
    return set(re.findall(r"[A-Za-z_]\w*", ty))


def holders(all_structs):
    """the names that hold a charge over their struct's other fields: Charge (and Held, read as Charge) only; a self-charged
    field type (ChargedVec, a struct with its own Charge) is no holder of its siblings' bytes"""
    return set(BASE_HOLDERS)


def violations(all_structs, h):
    """-> {(path, struct): (line, n heap fields before a charge-holding field)}"""
    out = {}
    for p, name, line, fields in all_structs:
        charge = [i for i, f in enumerate(fields) if _words(f) & h]
        if not charge:
            continue
        last = max(charge)
        early = [i for i, f in enumerate(fields) if i < last and i not in charge and HEAP.search(f)]
        if early:
            out[(p, name)] = (line, len(early))
    return out


def parse_debt(text):
    rows, bad = {}, []
    for n, raw in enumerate(text.split("\n"), 1):
        s = raw.split("#", 1)[0].rstrip()
        if not s.strip():
            continue
        f = s.split("\t")
        if len(f) < 4 or not all(x.strip() for x in f[:4]):
            bad.append((n, "a row is path<TAB>struct<TAB>owner lane<TAB>why it stays"))
            continue
        key = (f[0].strip(), f[1].strip())
        if key in rows:
            bad.append((n, f"duplicate row {key[0]} {key[1]}"))
        rows[key] = (n, f[2].strip())
    return rows, bad


def scan(ctx):
    alls = []
    for p in sources(ctx):
        t = ctx.read(p)
        if t is None or generated(t) or "struct" not in t:
            continue
        for name, line, fields in structs(Src(p, t)):
            alls.append((p, name, line, fields))
    return violations(alls, holders(alls))


def check_tree(ctx):
    out = []
    text, at = ctx.ledger(DEBT)
    rows, bad = parse_debt(text) if text is not None else ({}, [])
    out += [(*at(n), note) for n, note in bad]
    hits = scan(ctx)
    for (p, name), (n, k) in sorted(hits.items()):
        if (p, name) not in rows:
            out.append((p, n, f"`{name}` declares {k} heap-owning field(s) before its charge-holding field; instead: {INSTEAD}"))
    ctx.notes += [f"RCF: {ledger_loc(at, n)}: {p} `{name}` ({owner}) is in order (or gone): delete the row"
                  for (p, name), (n, owner) in sorted(rows.items(), key=lambda kv: kv[1][0]) if (p, name) not in hits]
    if rows:
        ctx.notes.append(f"RCF: CHARGE-ORDER-DEBT {len(rows)} rows")
    base_text = ctx.base_ledger(DEBT) if ctx.base_ledger else None
    if base_text is not None:
        base, _ = parse_debt(base_text)
        for (p, name), (n, owner) in sorted(rows.items(), key=lambda kv: kv[1][0]):
            if (p, name) not in base:
                out.append((*at(n), f"{p} `{name}` ({owner}) added: the list only shrinks (declare the Charge first)"))
    return out


# planted: the bytes before their charge; clean: the charge first (a holder found transitively, a tuple struct)
PLANTED_TOML = ("crates/backend/access/pgrcolumnar/pgrc_write/src/planted.rs",
                "pub struct Seal {\n    pub words: Vec<u64>,\n    charge: memledger::Charge,\n}\n")
CLEAN_TOML = ("crates/backend/access/pgrcolumnar/pgrc_write/src/clean.rs",
              "pub struct Seal {\n    charge: memledger::Charge,\n    pub words: Vec<u64>,\n    n: u32,\n}\n"
              "struct Bump(Held<u64>, Box<[u8]>);\n"
              "#[cfg(test)]\nmod t {\n    struct Late { v: Vec<u8>, c: Charge }\n}\n")
MORE_PLANTED_TOML = [
    ("crates/gravity/hosts/src/planted.rs",       # a tuple struct
     "struct Buf(Vec<u8>, Charge);\n"),
    ("crates/gravity/driver/src/planted.rs",      # a direct Charge after a map, among self-charged fields
     "pub struct Run<'a> {\n    rows: HashMap<u32, String>,\n    order: memledger::ChargedVec<u32>,\n    c: Option<Charge>,\n    a: &'a u8,\n}\n"),
]
MORE_CLEAN_TOML = [
    ("crates/gravity/hosts/src/clean.rs",         # no heap field before it; PgVec is a context's, not the heap's
     "struct Buf { a: u32, words: PgVec<'static, u64>, c: Charge, v: Vec<u8> }\n"),
    ("crates/gravity/driver/src/clean.rs",        # self-charged fields after heap ones: no direct Charge, no hit
     "struct Pin { c: Charge }\npub struct Run<'a> {\n    rows: HashMap<u32, String>,\n    pin: Option<Pin>,\n"
     "    order: memledger::ChargedVec<u32>,\n    a: &'a u8,\n}\n"),
]
