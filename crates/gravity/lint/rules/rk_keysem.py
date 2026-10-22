"""RK: a function outside the key-semantics home decides key equality, hash, order, site or epoch (PLAN-v5-keysem §5; the
user's go 2026-10-04 18:49Z: "one construct for user-defined key semantics"). The home is `KeySem`, gravity_fn::typesem
(fn/src/typesem/: keysem.rs, keysem/ (its user keys in keysem/user.rs) and the form deciders), with fn/src/cmpfn.rs and
fn/src/resolved.rs, which fold into it. A decision is found by construct, anywhere in the gravity sources outside the
home and outside the plan-time catalog (decompose/src/catalog/, its host impl hosts/src/facts.rs):
- a call of a typesem sub-decider (`typesem::{form, record_form, bucket_form, one_image, order_image, bits_form,
  word_order, op_order, own_order_family, default_btree_family, btree_equality, user_key}`, `user::user_key`), of a
  keysem internal (`keysem::{eq_form, word_eq, bits_decide, intern_op, hash_procs}`), of a site decider
  (`resolved::{native, native_key, leader_only}`) or anything under `cmpfn::`;
- a run-time call of a key support function past the bound-call table (`hosted_cmp`, `hosted_held_cmp`,
  `hosted_held_eq`, `hosted_pair`; not their definitions);
- a call of one of the plan-time key methods of `Facts` (`key_holds_multirange`, `key_eq`, `op_dir`, `op_family_dir`,
  `op_order`, `no_order_proc`, `op_family`, `op_hash_functions`, `op_image_eq`);
- a key OID list or support-proc OID (`IMAGE_EQ`, `INT_CROSS_EQ`, `HASH_NEEDS_COLLATION`, `INTEGER_HASH_FAMILY`,
  `F_HASH_*`, `F_BT*CMP`);
- a direct support lookup (`get_op_hash_functions`, `get_opfamily_proc`, a type cache `TYPECACHE_EQ_OPR` / `_CMP_PROC`
  / `_HASH_PROC` request);
- a LANGUAGE internal alias lookup (`fmgr_internal_function`, `fmgr_lookup_by_name`, `internal_builtin_oid`,
  `internal_builtin_of`): which built-in a support proc runs is decided once, by `keysem::body` (equivclass#53), and
  read through it (`Facts::builtin_body`, `effects::body`), kind site.

ERROR MODE (slice 7, PLAN-v5-keysem §5 / §6: the ledger reached 0 rows). The registry, lint/KEYSEM-DEBT (a directory, one
row per file), lists each such function by path and name with its owner lane and kind (eq, hash, order, site, epoch,
error; several joined by '+'); a row covers that file's functions of that name. A detected function with no row FAILS
(slices 0-6 ran in ledger mode, where it was a warning); with the ledger empty, a new decision is made in the home. On
a landing (--base) a row the parent lacks fails (the list only shrinks), unless it replaces a row of the same function
name the landing removes (a certified move or split), or the parent has no ledger yet. A row whose function its file no
longer defines is a note (delete it). Module-level constants are not charged (only uses inside a function are)."""
import re

from common import G, ledger_loc, Src, generated, in_any, in_src, walk_sources
from rl_lifetime import functions

ID = "RK"
TITLE = "a function outside the key-semantics home decides key equality, hash, order, site or epoch"
INSTEAD = "gravity_fn::typesem (KeySem: resolve a KeyReq once; its eq / hash / order / site), keysem::call for run-time " \
          "support calls; not yet moved: a lint/KEYSEM-DEBT row"
GLOBAL = True
DEBT = G + "lint/KEYSEM-DEBT"
_HOME = [G + "fn/src/typesem/", G + "fn/src/cmpfn.rs", G + "fn/src/resolved.rs"]
_CATALOG = [G + "decompose/src/catalog/", G + "hosts/src/facts.rs"]
SCOPE = in_src

KINDS = {"eq", "hash", "order", "site", "epoch", "error"}
_SUB = (r"form|record_form|bucket_form|one_image|order_image|bits_form|word_order|op_order|own_order_family"
        r"|default_btree_family|btree_equality|user_key")
# (kind, pattern): the first kind a function's constructs name, in this order, then the others joined by '+'
USES = [
    ("site", re.compile(r"\bresolved::(?:native|native_key|leader_only)\s*\(|(?<!fn )\b(?:fmgr_internal_function|fmgr_lookup_by_name)\s*\("
                        r"|\binternal_builtin_(?:oid|of)\b")),
    ("order", re.compile(r"\bcmpfn::|(?<!fn )\b(?:hosted_cmp|hosted_held_cmp|hosted_pair)\s*\(|\btypesem::(?:order_image|word_order"
                         r"|op_order|own_order_family|default_btree_family|bits_form)\s*\(|\.(?:op_dir|op_family_dir|op_order"
                         r"|no_order_proc|op_family)\s*\(|\bF_BT[A-Z]*CMP\b|(?<!fn )\bget_opfamily_proc\s*\(|\bTYPECACHE_CMP_PROC\b")),
    ("hash", re.compile(r"\bkeysem::hash_procs\b|\.op_hash_functions\s*\(|\b(?:HASH_NEEDS_COLLATION|INTEGER_HASH_FAMILY)\b"
                        r"|\bF_HASH_[A-Z]+\b|(?<!fn )\bget_op_hash_functions\s*\(|\bTYPECACHE_HASH_PROC\b")),
    ("eq", re.compile(r"\btypesem::(?:form|record_form|bucket_form|one_image|btree_equality|user_key)\s*\(|\buser::user_key\b"
                      r"|\bkeysem::(?:eq_form|word_eq|bits_decide|intern_op)\b|(?<!fn )\bhosted_held_eq\s*\("
                      r"|\.(?:key_eq|key_holds_multirange|op_image_eq)\s*\(|\b(?:IMAGE_EQ|INT_CROSS_EQ)\b|\bTYPECACHE_EQ_OPR\b")),
]
_FACTS = re.compile(r"\.(?:key_holds_multirange|key_eq|op_dir|op_family_dir|op_order|no_order_proc|op_family|op_hash_functions|op_image_eq)\s*\(")
_NEEDS = re.compile(r"typesem::|user::user_key|keysem::|resolved::|cmpfn::|hosted_(?:cmp|held_cmp|held_eq|pair)|key_holds_multirange"
                    r"|fmgr_internal_function|fmgr_lookup_by_name|internal_builtin_"
                    r"|key_eq|op_image_eq|op_dir|op_family|op_order|no_order_proc|op_hash_functions|IMAGE_EQ|INT_CROSS_EQ|HASH_NEEDS_COLLATION"
                    r"|INTEGER_HASH_FAMILY|F_HASH_|F_BT|get_op_hash_functions|get_opfamily_proc|TYPECACHE_(?:EQ_OPR|CMP_PROC|HASH_PROC)")


def is_home(p):
    return in_any(p, _HOME)


def deciders(src):
    """-> {(name, line): set of kinds} of the functions `src` defines that decide a key's eq / hash / order / site (see
    the module doc); a construct is charged to the innermost function holding it."""
    t, fns = src.joined("code"), functions(src)
    catalog = in_any(src.path, _CATALOG)
    out = {}
    for kind, pat in USES:
        for m in pat.finditer(t):
            if catalog and _FACTS.match(t, m.start()):
                continue
            inner = [(n, line, b - a) for n, line, (a, b) in fns if a <= m.start() < b]
            if inner:
                n, line, _ = min(inner, key=lambda x: x[2])
                out.setdefault((n, line), set()).add(kind)
    return out


def defined(src):
    return {n for n, _, _ in functions(src)}


def parse_debt(text):
    """-> ({(path, fn): (line, owner, kind)}, [(line, problem)])."""
    rows, bad = {}, []
    for n, raw in enumerate(text.split("\n"), 1):
        s = raw.split("#", 1)[0].rstrip()
        if not s.strip():
            continue
        f = s.split("\t")
        if len(f) < 4 or not all(x.strip() for x in f[:4]):
            bad.append((n, "a row is path<TAB>fn<TAB>owner lane<TAB>kind[+kind..]<TAB>what it decides"))
            continue
        kind = f[3].strip()
        if not set(kind.split("+")) <= KINDS:
            bad.append((n, f"kind {kind!r} is not one or more of {', '.join(sorted(KINDS))} joined by '+'"))
            continue
        key = (f[0].strip(), f[1].strip())
        if key in rows:
            bad.append((n, f"duplicate row {key[0]} {key[1]}"))
        rows[key] = (n, f[2].strip(), kind)
    return rows, bad


def scan(ctx):
    """-> ({(path, fn): (line, kinds)}, {path: defined names}) over the tree, rows' files included."""
    text, _ = ctx.ledger(DEBT)
    rows, _ = parse_debt(text) if text is not None else ({}, [])
    listed = {p for p, _ in rows}
    hits, files = {}, {}
    for p in walk_sources(ctx.root):
        if not SCOPE(p) or is_home(p):
            continue
        t = ctx.read(p)
        if t is None or generated(t) or not (_NEEDS.search(t) or p in listed):
            continue
        src = Src(p, t)
        if p in listed:
            files[p] = defined(src)
        for (name, line), kinds in deciders(src).items():
            h = hits.setdefault((p, name), (line, set()))
            h[1].update(kinds)
    return hits, files


def check_tree(ctx):
    out = []
    text, at = ctx.ledger(DEBT)
    rows, bad = parse_debt(text) if text is not None else ({}, [])
    hits, files = scan(ctx)
    out += [(*at(n), note) for n, note in bad]
    # error mode (slice 7): an unlisted decider fails
    for (p, f), (n, kinds) in sorted(hits.items()):
        if (p, f) not in rows:
            out.append((p, n, f"`{f}` decides a key's {'+'.join(sorted(kinds))} outside the key-semantics home; instead: {INSTEAD}"))
    stale = [f"{ledger_loc(at, n)}: {p} `{f}` ({owner}) is not in its file: delete the row once it is gone (an entry queued ahead may add it)"
             for (p, f), (n, owner, _) in sorted(rows.items(), key=lambda kv: kv[1][0])
             if (p, f) not in hits and f not in files.get(p, set())]
    ctx.notes += [f"RK: {s}" for s in stale]
    base_text = ctx.base_ledger(DEBT) if ctx.base_ledger else None
    if base_text is not None:
        base, _ = parse_debt(base_text)
        removed = {bf for bp, bf in base if (bp, bf) not in rows}
        for (p, f), (n, owner, _) in sorted(rows.items(), key=lambda kv: kv[1][0]):
            if (p, f) not in base and f not in removed:
                out.append((*at(n), f"{p} `{f}` ({owner}) added: the list only shrinks (decide it in the key-semantics home)"))
    return out


# error mode: the planted failure is an unlisted decider (a direction read past KeySem, `Facts::op_dir`)
PLANTED_TOML = (G + "decompose/src/ops/sort/planted.rs", "fn sort_key_dir(f: &dyn Facts, op: u32) -> bool { f.op_dir(op).is_some() }\n")
CLEAN_TOML = (G + "decompose/src/ops/sort/clean.rs",
              "fn sort_key(f: &dyn Facts, r: &KeyReq) -> bool { f.key(r).is_ok() }\n"
              "fn keyed(k: &KeySem) -> bool { matches!(k.order, Order::Word { .. }) }\n")
