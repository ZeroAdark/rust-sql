"""RX: a function outside the key-image home names a key image (C5 (b), reports/BUG-RECURRENCE-20261011.md: D3208, D3240,
D3839-41, D3848, D3857, producer and consumer image widths decided apart). A key's image (how its words are built from
the value: raw bits, a float's folded bits, a numeric mantissa at a scale, a text fingerprint, a canonical form) is to be
decided ONCE per (column, consumer set) in keysem, and read there by both the producer (a scan's lane, a packed pgrc
lane, a hashed emit) and the consumer (a group, a set operation, a join). The home is keysem's two halves: the plan-time
catalog's decompose/src/catalog/keysem/ (image.rs: the image types and the decisions over `Facts` / `TypeFacts`; decompose
reads no gravity_fn) and gravity_fn::typesem (the catalog-reading half the host answers `Facts` from). Until then the
image is named in decompose
(`WordImage` in catalog/typefacts.rs, `KeyImage` in ops/shared/sort.rs, the two encodings of one decision), lowerv4 and
hosts. A function that names one is charged here:
- the image types and their helpers: `KeyImage`, `WordImage`, `KeyEq`, `one_image`, `order_image`, `images_pair`,
  `name_text`, `one_integer`, `same_rep`, `bucket_image`, `string_pair`.
Module-level items (a type or field declaration, a `use`) are not charged; a function body is. A call of a home
decision by its keysem path (`keysem::one_image(..)`, `keysem::image::order_image(..)`) reads the one decision and is not
charged; the image types' names (`KeyImage`, `WordImage`, `KeyEq`) are, by any path. An `as` alias of one
(`use ..::KeyImage as DeclImage;`, `use KeyImage as I;`) is charged as the name itself, in the file that declares it:
until the 10-11 recount the aliases hid ~15 functions (keyimg.rs `packed_set_key`, the D3839 / D3857 sites among them).

The registry, lint/KEYIMAGE-DEBT (a directory, one row per file), lists each such function by path and name with its
owner lane and kind (decide: it builds or maps an image; read: it reads a decided one to emit or compare), seeded from
trunk. A charged function with no row fails. On a landing (--base) a row the parent lacks fails (the list only shrinks),
unless it replaces a row of the same function name the landing removes (a certified move or split). A row whose
function its file no longer defines (or no longer names an image) is a note: delete it. KEYIMAGE-DEBT reaches 0 when
every image is decided in the home and read through it."""
import re

from common import G, ledger_loc, Src, generated, in_any, in_src, walk_sources
from rl_lifetime import functions

ID = "RX"
TITLE = "a function outside the key-image home names a key image"
INSTEAD = "the one key-image decision in keysem (decompose catalog/keysem/image.rs, gravity_fn::typesem; read by producer " \
          "and consumer); not yet moved: a lint/KEYIMAGE-DEBT row"
GLOBAL = True
DEBT = G + "lint/KEYIMAGE-DEBT"
_HOME = [G + "fn/src/typesem/", G + "decompose/src/catalog/keysem/"]
SCOPE = in_src

KINDS = {"decide", "read"}
IDS = ("KeyImage", "WordImage", "KeyEq", "one_image", "order_image", "images_pair", "name_text", "one_integer", "same_rep",
       "bucket_image", "string_pair")
_USE = re.compile(r"\b(?:" + "|".join(IDS) + r")\b")
_DECIDE = re.compile(r"\b(?:KeyImage|WordImage)::of\s*\(|\bkey_of\s*\(|\b(?:one_image|order_image|images_pair|bucket_image)\s*\(")


def is_home(p):
    return in_any(p, _HOME)


_ALIAS = re.compile(r"\b(" + "|".join(IDS) + r")\s+as\s+([A-Za-z_]\w*)")


def patterns(t):
    """-> (use, decide): `_USE` / `_DECIDE`, widened by the file's `as` aliases of an id (each alias stands for its id)."""
    al = {}
    for m in _ALIAS.finditer(t):
        if m.group(2) != "_":
            al.setdefault(m.group(1), set()).add(m.group(2))
    if not al:
        return _USE, _DECIDE
    names = sorted(set(IDS) | {a for v in al.values() for a in v})
    types = sorted({"KeyImage", "WordImage"} | al.get("KeyImage", set()) | al.get("WordImage", set()))
    fns = sorted({"one_image", "order_image", "images_pair", "bucket_image"}
                 | {a for k in ("one_image", "order_image", "images_pair", "bucket_image") for a in al.get(k, ())})
    use = re.compile(r"\b(?:" + "|".join(map(re.escape, names)) + r")\b")
    decide = re.compile(r"\b(?:" + "|".join(map(re.escape, types)) + r")::of\s*\(|\bkey_of\s*\(|\b(?:"
                        + "|".join(map(re.escape, fns)) + r")\s*\(")
    return use, decide


# a home decision called by its keysem path: the reader asks the home (the types' names stay charged by any path)
_ASKS = re.compile(r"\bkeysem::(?:image::)?(?:one_image|order_image|images_pair|name_text|one_integer|same_rep|bucket_image"
                   r"|string_pair)\s*\(")


def charged(src):
    """-> {(name, line): kind} of the functions `src` defines whose body names a key image (the innermost one); a home
    decision called by its keysem path (`_ASKS`) is not a naming."""
    t, fns = src.joined("code"), functions(src)
    asks = [(m.start(), m.end()) for m in _ASKS.finditer(t)]
    t = "".join(c if not any(a <= i < b for a, b in asks) else " " for i, c in enumerate(t)) if asks else t
    use, decide = patterns(t)
    out = {}
    for m in use.finditer(t):
        inner = [(n, line, b - a, (a, b)) for n, line, (a, b) in fns if a <= m.start() < b]
        if inner:
            n, line, _, (a, b) = min(inner, key=lambda x: x[2])
            out[(n, line)] = "decide" if decide.search(t, a, b) else out.get((n, line), "read")
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
            bad.append((n, "a row is path<TAB>fn<TAB>owner lane<TAB>kind<TAB>what it names"))
            continue
        kind = f[3].strip()
        if kind not in KINDS:
            bad.append((n, f"kind {kind!r} is not one of {', '.join(sorted(KINDS))}"))
            continue
        key = (f[0].strip(), f[1].strip())
        if key in rows:
            bad.append((n, f"duplicate row {key[0]} {key[1]}"))
        rows[key] = (n, f[2].strip(), kind)
    return rows, bad


def scan(ctx, rows):
    hits, files = {}, {}
    listed = {p for p, _ in rows}
    for p in walk_sources(ctx.root):
        if not SCOPE(p) or is_home(p):
            continue
        t = ctx.read(p)
        if t is None or generated(t) or not (_USE.search(t) or p in listed):
            continue
        src = Src(p, t)
        if p in listed:
            files[p] = defined(src)
        for (name, line), kind in charged(src).items():
            hits.setdefault((p, name), (line, kind))
    return hits, files


def check_tree(ctx):
    out = []
    text, at = ctx.ledger(DEBT)
    rows, bad = parse_debt(text) if text is not None else ({}, [])
    hits, files = scan(ctx, rows)
    out += [(*at(n), note) for n, note in bad]
    for (p, f), (n, kind) in sorted(hits.items()):
        if (p, f) not in rows:
            out.append((p, n, f"`{f}` names a key image outside the key-image home ({kind}); instead: {INSTEAD}"))
    stale = [f"{ledger_loc(at, n)}: {p} `{f}` ({owner}) names no key image now: delete the row (an entry queued ahead may add it)"
             for (p, f), (n, owner, _) in sorted(rows.items(), key=lambda kv: kv[1][0]) if (p, f) not in hits]
    ctx.notes += [f"RX: {s}" for s in stale]
    base_text = ctx.base_ledger(DEBT) if ctx.base_ledger else None
    if base_text is not None:
        base, _ = parse_debt(base_text)
        removed = {bf for bp, bf in base if (bp, bf) not in rows}
        recount = recount_rows(text, ctx)
        for (p, f), (n, owner, _) in sorted(rows.items(), key=lambda kv: kv[1][0]):
            if (p, f) not in base and f not in removed and (p, f) not in recount:
                out.append((*at(n), f"{p} `{f}` ({owner}) added: the list only shrinks (decide the image in typesem)"))
    return out


RECOUNT = "recount: hidden by alias, not regrowth"


def recount_rows(text, ctx):
    """The alias recount (once): the rows marked RECOUNT a landing may add, only while the base's RX is the one that did
    not see through an `as` alias (its source lacks `_ALIAS`). Once this RX is the base, the set is
    empty: the list only shrinks again, with no exception."""
    rd = getattr(ctx, "base_read", None)
    # the base's key-image rule: rx_keyimage.py, or ri_keyimage.py (its name before the id split)
    src = next((t for t in (rd(G + "lint/rules/rx_keyimage.py"), rd(G + "lint/rules/ri_keyimage.py")) if t is not None), None) if rd else None
    if src is None or "_ALIAS" in src or text is None:
        return set()
    out = set()
    for raw in text.split("\n"):
        f = raw.split("#", 1)[0].rstrip().split("\t")
        if len(f) >= 5 and RECOUNT in f[4]:
            out.add((f[0].strip(), f[1].strip()))
    return out


# the planted failure is a new function that picks a key's image itself
PLANTED_TOML = (G + "lowerv4/src/ops/setop/planted.rs",
                "fn side_image(t: &TypeFacts) -> KeyImage { KeyImage::of(t.image, 0) }\n")
CLEAN_TOML = (G + "fn/src/typesem/clean.rs", "pub fn side_image(t: &TypeFacts) -> KeyImage { KeyImage::of(t.image, 0) }\n")
# an alias hides nothing: a body naming the image only through `KeyImage as DeclImage` is charged (the 10-11 recount);
# an alias of a name that is not an id is not
# a reader that asks the home decision by its keysem path is clean (MORE_CLEAN_TOML); one that calls it bare (an
# import) is not
MORE_PLANTED_TOML = [(G + "decompose/src/ops/agg/planted_bare.rs",
                      "use crate::catalog::keysem::one_image;\nfn pinned(f: &dyn Facts, t: u32) -> bool {\n    one_image(f, t, 0)\n}\n"),
                     (G + "lowerv4/src/table/planted_alias.rs",
                      "use gravity_decompose::ops::shared::sort::KeyImage as DeclImage;\n"
                      "fn wide(k: &KeyDecl) -> bool {\n    k.image == DeclImage::NumMantissa && k.words == 1\n}\n")]
MORE_CLEAN_TOML = [(G + "lowerv4/src/table/clean_alias.rs",
                    "use crate::table::keyimg::ImageWords as Img;\n"
                    "fn width(i: &Img) -> usize {\n    i.words.words.len()\n}\n"),
                   (G + "decompose/src/ops/agg/clean_asks.rs",
                    "fn pinned(f: &dyn Facts, t: u32) -> bool {\n    crate::catalog::keysem::one_image(f, t, 0)\n}\n")]
