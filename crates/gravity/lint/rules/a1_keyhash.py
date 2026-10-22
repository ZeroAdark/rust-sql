"""A1: KeyHash is the only hash access (constructs #28; the 09-23 hash ruling). Ported from
lowerv4/tests/m0_keyhash_only.rs (keyhash_is_the_only_hash_access), whose scope was lowerv4 and the v4 hubs; here it is
every directory outside LEGACY. A file may take from gravity_abi::hash / gravity_expr::emit::hash only the interface,
never write the value of a module-level 64-bit const of those files, never build `KeyHash(..)` or read `.0` of a hash;
lowering never carries KeyHash's ABI word, and never shifts a KeyHash emit's result by 32..63 by hand."""
import re
from common import G, home, in_src, is_lowering, word_start

ID = "A1"
TITLE = "raw hash access past KeyHash"
INSTEAD = "KeyHash's methods / the route-load helpers (runtime); emit_* (IR half)"
SCOPE = in_src
HOME = home("keyhash")
RUNTIME_API = {"KeyHash", "Depth", "table_fits", "route_and_full", "build_cap_bits", "agg_grow_at", "agg_cap_bits",
               "FpKey", "fp128", "Fnv128", "Mix128", "phi_top", "phi_fold", "self"}   # Fnv128: a program's content identity
# hash (program/mod.rs); Mix128: the code cache's content key (backend cache.rs); phi_top: the program cache's shard pick;
# phi_fold: the floor prune's group set (kernels topn parthaving.rs)
IR_API = {"KeyPart", "ImageValues", "Bits", "self"}
RUNTIME_API |= {"hash_bytes64"}   # a byte string's content identity hash (dict's derived / bits tables, a per-code fingerprint)
_WORD = re.compile(r"[A-Za-z0-9_]+")
_SHIFT = re.compile(r"(?:Ushr|Sshr|Shl|>>|<<)")


def _big(w):
    w = w.replace("_", "")
    for suf in ("u64", "i64"):
        if w.endswith(suf):
            w = w[:-3]
    try:
        v = int(w, 16) if w[:2] in ("0x", "0X") else (int(w) if w.isdigit() else None)
    except ValueError:
        return None
    return v if v is not None and v > 0xFFFFFFFF else None


def _literals(ctx):
    if "a1_lits" not in ctx.cache:
        lits = set()
        for f in ("abi/src/hash.rs", "expr/src/emit/hash.rs"):
            t = ctx.read(G + f) or ""
            for l in t.split("\n"):
                if re.match(r"(?:pub(?:\(crate\))?\s+)?const\s", l):
                    lits |= {v for v in map(_big, _WORD.findall(l)) if v}
        ctx.cache["a1_lits"] = lits
    return ctx.cache["a1_lits"]


def _names(body, module):
    """The names a file takes from `module`: paths through it, `use` groups, and `hash::x` once the module is imported."""
    imported = ("use " + module + ";") in body
    prefix, out = module + "::", []
    for n, line in enumerate(body.split("\n"), 1):
        l = line.replace(" as ", "@").replace(" ", "")
        ats = [m.end() for m in re.finditer(re.escape(prefix), l)]
        if imported:
            ats += [m.end() for m in re.finditer(r"(?<!::)hash::", l)]
        for at in ats:
            rest = l[at:]
            if rest.startswith("{"):
                names = [x.split("@")[0] for x in rest[1:].split("}")[0].split(",")]
            else:
                names = [re.match(r"[A-Za-z0-9_*]*", rest).group()]
            out += [(n, x) for x in names if x]
    return out


_LONG = re.compile(r"(?:0[xX][0-9a-fA-F_]{9,}|[0-9][0-9_]{9,})(?:u64|i64)?\b")
_KH = re.compile(r"KeyHash\s*\(")
_DOT0 = re.compile(r"\.\s*0(?![0-9])")


def _hash_dot0(t, m):
    """`.0` on an identifier naming a hash (key_hash.0, h.hash.0)."""
    k = m.start()
    j = k
    while j > 0 and (t[j - 1].isalnum() or t[j - 1] == "_"):
        j -= 1
    return "hash" in t[j:k].lower()
_EMIT = re.compile(r"emit_key_hash\(|emit_route_and_full\(")
_ABI = re.compile(r"KeyHash\s*::\s*(?:to_abi|from_abi)\b")


def check(src, ctx):
    body = src.joined()
    lits, out = _literals(ctx), []
    if "hash::" in body:
        for module, ir in (("gravity_abi::hash", False), ("gravity_expr::emit::hash", True)):
            if module.split("::")[-1] + "::" not in body and module not in body:
                continue
            for n, name in _names(body, module):
                ok = (name.startswith("emit_") or name in IR_API) if ir else name in RUNTIME_API
                if not ok:
                    out.append(n)
    for n in src.hits(_LONG, keep=word_start):
        if any(_big(w) in lits for w in _LONG.findall(src.code[n - 1])):
            out.append(n)
    out += src.hits(_KH) + src.hits(_DOT0, keep=_hash_dot0)
    if is_lowering(src.path):
        out += src.hits(_ABI)
        for e in src.hits(_EMIT):
            for n in range(e + 1, min(e + 7, len(src.code) + 1)):
                l = src.code[n - 1].replace(" ", "")
                if re.search(r"(?:Ushr|Sshr|Shl|>>|<<)", l) and re.search(r"(?:,|>>|<<)(?:3[2-9]|[45][0-9]|6[0-3])\)?", l):
                    out.append(n)
    return sorted(set(out))


NEEDS = ("hash", "Hash", "HASH")   # any of these in the raw text, or the file is skipped
PLANTED = [(G + "lowerv4/src/ops/agg/planted.rs",
            "use gravity_abi::hash::{KeyHash, PHI64};\npub fn f(key_hash: KeyHash) -> u64 { key_hash.0 }\n")]
CLEAN = [(G + "lowerv4/src/ops/agg/clean.rs",
          "use gravity_abi::hash::{KeyHash, Depth};\nuse gravity_expr::emit::hash::{emit_key_hash, Bits};\n"
          "pub fn f(b: &mut B, img: &Img) -> V { emit_key_hash(b, img) } // PHI64 in a comment\n")]
