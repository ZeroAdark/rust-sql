"""RY: a key image of two or more words built on the stack as a plain `[u64; N]` and handed to the key path (D4684, D4691).
A plain array takes whatever 8-byte slot its frame gives it; the pair stores that write it are 16 bytes, and at an 8-mod-16
slot one of them straddles a 4 KiB page at 1 stack position in 256. Every reload of the image (the hash, the probe's
compare) then waits on the split store: D4684, +38 % cycles on a grouped aggregate at equal instructions, when a 40-byte thread_local
moved every thread's stack top down 32 bytes and front_light's `[u64; 3]` image landed on a page boundary. An image of a
16-byte-aligned type never crosses a page. Charged, in a function body:
- a local `let x = [a, b, ..]` / `[v; N]` (N >= 2 or a const) / `let x: [u64; N] = ..` / `let x = f(..)` where `f`
  returns `[u64; N]` (N >= 2 or a const, any file of the tree), or such an `f(..)` call itself,
- named by reference (`&x`, `&x[..n]`, `x.as_slice()`, a `let y = &x[..n]` alias of it) in the arguments of
  `KeyHash::of`, a `probe`, `prefetch_home` or `route` call, or of a call of a function the tree declares with a
  `keys` / `key` / `img` / `image: &[u64]` parameter (the key path: fold_code -> record -> KeyHash::of).
A one-word key (`[k]`, 8 bytes) cannot straddle and is not charged."""
import re

from common import G, in_src, walk_sources
from rl_lifetime import functions

ID = "RY"
TITLE = "a stack key image of two or more words that is not a 16-byte-aligned type"
INSTEAD = "gravity_kernels::table::image::AlignedImage<N> (repr(align(16)); `.as_ref()` reads it as a slice)"
HOME = []
SCOPE = in_src
NEEDS = ("KeyHash", "probe", "prefetch_home", "route", "keys", "key", "img", "image")

_N = r"(?:[2-9]|[1-9][0-9]+|[A-Z][A-Z0-9_]*)"
_ARR = r"\[\s*u64\s*;\s*" + _N + r"\s*\]"
_ALIAS = re.compile(r"\btype\s+([A-Z][A-Za-z0-9_]*)\s*=\s*" + _ARR + r"\s*;")
_RET = r"\bfn\s+([a-z_][a-z0-9_]*)\s*(?:<[^>]*>)?\s*\([^)]*\)\s*->\s*"
_KEYS_PARAM = re.compile(r"\bfn\s+([a-z_][a-z0-9_]*)\s*(?:<[^>]*>)?\s*\([^)]*\b(?:keys|key|img|image)\s*:\s*&\s*\[\s*u64\s*\]")
_LET = re.compile(r"\blet\s+(?:mut\s+)?([a-z_][a-z0-9_]*)\s*(?::\s*\[\s*u64\s*;\s*([^\]]+?)\s*\])?\s*=\s*")
_HEAD = re.compile(r"(?:[A-Za-z_][A-Za-z0-9_]*\s*(?:::|\.)\s*)*([a-z_][a-z0-9_]*)\s*\(")
_FIXED = ("KeyHash", "probe", "prefetch_home", "route")
# names std and every layer share: a call by one of these names is not read as the key path
_COMMON = {"new", "of", "get", "find", "build", "run", "iter", "read", "put", "has", "own", "generic", "mix", "floor",
           "holds", "grown", "offer", "offered", "locate", "append"}


def _tree_names(ctx):
    """-> (producers, consumers): the function names the tree declares returning `[u64; N >= 2]` (or a `type X = [u64; N]`
    alias of one, `Self::X` included: a trait's image type), and those taking a key slice parameter. Read once per run
    from every production file (raw text: a test helper's name only widens the set)."""
    if "ry_names" not in ctx.cache:
        texts = [ctx.read(p) or "" for p in walk_sources(ctx.root)]
        texts = [t for t in texts if "u64" in t]
        aliases = set()
        for t in texts:
            aliases |= set(_ALIAS.findall(t))
        ret = _ARR + "".join(r"|(?:Self\s*::\s*)?" + a + r"\b" for a in sorted(aliases))
        producer = re.compile(_RET + r"(?:" + ret + r")")
        prod, cons = set(), set()
        for t in texts:
            prod |= set(producer.findall(t))
            cons |= set(_KEYS_PARAM.findall(t))
        ctx.cache["ry_names"] = (prod, cons)
    return ctx.cache["ry_names"]


def _close(t, i, o, c):
    """-> the index after the bracket closing the one at t[i] (`o`), or -1."""
    depth = 0
    while i < len(t):
        if t[i] == o:
            depth += 1
        elif t[i] == c:
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return -1


def _words(lit):
    """`lit` = an array literal's inside -> True when it holds two or more words: `[v; N]` with N >= 2 (or a const), or
    two or more top-level elements."""
    depth, commas, semi = 0, 0, -1
    for i, ch in enumerate(lit):
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        elif depth == 0 and ch == ";":
            semi = i
        elif depth == 0 and ch == ",":
            commas += 1
    if semi >= 0:
        return re.fullmatch(r"\s*" + _N + r"\s*", lit[semi + 1:]) is not None
    return commas >= 2 or (commas == 1 and lit.rstrip()[-1:] != ",")


def _arrays(body, prod):
    """-> (arrays, aliases): the names a function body binds to a stack array of two or more words, and the names it
    binds to a reference into one (`let keys = &img[..nk]`, `&img.as_ref()[..nk]`)."""
    arrays, aliases = set(), set()
    for m in _LET.finditer(body):
        name, n, i = m.group(1), m.group(2), m.end()
        if n is not None and re.fullmatch(_N, n):
            arrays.add(name)
        elif body.startswith("[", i):
            j = _close(body, i, "[", "]")
            if j > 0 and _words(body[i + 1:j - 1]):
                arrays.add(name)
        else:
            c = _HEAD.match(body, i)
            if c and c.group(1) in prod:
                arrays.add(name)
    for m in _LET.finditer(body):
        a = re.match(r"&\s*(?:mut\s+)?([a-z_][a-z0-9_]*)\b(?!\s*\[\s*[^\].]*\])", body[m.end():])
        if a and a.group(1) in arrays:
            aliases.add(m.group(1))
    return arrays, aliases


def _args(text):
    """-> a call's top-level arguments, closures left out (a closure's body is its own code, not an argument's value)."""
    out, depth, at = [], 0, 0
    for i, ch in enumerate(text + ","):
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        elif ch == "," and depth == 0:
            a = text[at:i].strip()
            if a and not re.match(r"(?:&\s*(?:mut\s+)?)?(?:move\s*)?\|", a):
                out.append(a)
            at = i + 1
    return out


def _slices(args, arrays, aliases, prod):
    """The call's arguments name a tracked array by reference (or a reference alias of one), or a producer's result."""
    args = "\n".join(_args(args))
    for x in arrays:
        if re.search(r"(?<!&)&(?!&)\s*(?:mut\s+)?" + x + r"\b(?!\s*(?:\[\s*[^\].]*\]|\())", args) or \
                re.search(r"(?<![A-Za-z0-9_.])" + x + r"\s*\.\s*as_(?:slice|ref)\s*\(", args):
            return True
    for x in aliases:
        if re.search(r"(?<![A-Za-z0-9_.])" + x + r"\b(?!\s*(?:\[\s*[^\].]*\]|\(|\.\s*[a-z_]))", args):
            return True
    for m in re.finditer(r"(?<!&)&(?!&)\s*(?:[A-Za-z_][A-Za-z0-9_]*\s*(?:::|\.)\s*)*([a-z_][a-z0-9_]*)\s*\(", args):
        if m.group(1) in prod:
            return True
    return False


def check(src, ctx):
    prod, cons = _tree_names(ctx)
    t, out = src.joined("code"), []
    call = re.compile(r"(?<![A-Za-z0-9_])(?:KeyHash\s*::\s*of|probe|prefetch_home|route" +
                      "".join("|" + re.escape(c) for c in sorted(cons - _COMMON) if c not in _FIXED) + r")\s*\(")
    for _name, _line, (a, b) in functions(src):
        body = t[a:b]
        arrays, aliases = _arrays(body, prod)
        for m in call.finditer(body):
            j = _close(body, m.end() - 1, "(", ")")
            if j > 0 and _slices(body[m.end():j - 1], arrays, aliases, prod):
                out.append(t.count("\n", 0, a + m.start()) + 1)
    return sorted(set(out))


PLANTED = [
    (G + "kernels/src/dict/strategies/planted.rs",           # D4684's form: a trait's image type, a method producer
     "pub trait CountIndex { type Img: AsRef<[u64]>; unsafe fn image(self, i: u32) -> Self::Img; }\n"
     "impl CountIndex for One {\n    type Img = [u64; 3];\n    unsafe fn image(self, i: u32) -> Self::Img {\n"
     "        let fp = *self.fps.add(i as usize);\n        [fp[0], fp[1], 0]\n    }\n}\n"
     "unsafe fn walk_by<I: CountIndex>(ix: I, t: *mut T1, code: u32, nk: usize) -> Option<u32> {\n"
     "    let img = ix.image(code);\n    let keys = &img.as_ref()[..nk];\n    T1::probe(t, keys, KeyHash::of(keys))\n}\n"),
    (G + "kernels/src/dict/strategies/planted.rs",
     "unsafe fn walk(t: *mut T1, fp: [u64; 2], nk: usize) {\n    let img = [fp[0], fp[1], 0];\n"
     "    T1::probe(t, &img[..nk], KeyHash::of(&img[..nk]));\n}\n"),
    (G + "kernels/src/dict/strategies/planted.rs",
     "fn image(k: [u64; 2], part: u64) -> [u64; 4] {\n    [k[0], k[1], part, 0]\n}\n"
     "unsafe fn fold_code(a: *mut A, keys: &[u64]) -> u64 {\n    (*a).find(keys)\n}\n"
     "unsafe fn settle(a: *mut A, k: [u64; 2], nk: usize) -> u64 {\n    fold_code(a, &image(k, 0)[..nk])\n}\n"),
    (G + "kernels/src/dict/strategies/planted.rs",
     "fn image(k: [u64; 2], part: u64) -> [u64; 4] {\n    [k[0], k[1], part, 0]\n}\n"
     "unsafe fn record(a: *mut A, keys: &[u64]) -> u64 {\n    (*a).find(keys)\n}\n"
     "unsafe fn flush(a: *mut A, e: [u64; 2], n: usize) -> u64 {\n    let keys = image(e, 0);\n    record(a, &keys[..n])\n}\n"),
    (G + "kernels/src/table/planted.rs",
     "unsafe fn seed(t: *mut T1) {\n    let mut w = [0u64; IMG_MAX];\n    w[0] = 7;\n    let keys = &w[..2];\n"
     "    (*t).prefetch_home(KeyHash::of(keys));\n}\n"),
]
CLEAN = [
    (G + "kernels/src/dict/strategies/clean.rs",
     "unsafe fn walk(t: *mut T1, fp: [u64; 2], nk: usize) {\n    let img = AlignedImage([fp[0], fp[1], 0]);\n"
     "    T1::probe(t, &img.as_ref()[..nk], KeyHash::of(&img.as_ref()[..nk]));\n}\n"),
    (G + "kernels/src/par_distinct/clean.rs",
     "unsafe fn one(t: *mut T1, r: Rec) -> bool {\n    let keys = [r.key(0)];\n    T1::probe(t, &keys, KeyHash::of(&keys)).is_some()\n}\n"),
    (G + "kernels/src/ops/sort/clean.rs",
     "fn params(n: u64) -> [u64; 4] {\n    [n, 0, 0, 0]\n}\nfn plan(n: u64) -> u64 {\n    let p = params(n);\n    p[0] + p[3]\n}\n"),
    (G + "kernels/src/table/clean.rs",
     "unsafe fn fold(t: *mut T1, keys: &[u64]) -> Option<u32> {\n    T1::probe(t, keys, KeyHash::of(keys))\n}\n"),
]
