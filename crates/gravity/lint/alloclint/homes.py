"""alloclint's homes (lane alloclint round 2, 2026-10-14; reports/ALLOCLINT-GRAVITY-20261014.md section 2): the code that
IS a memory home, or runs inside one, so the detector's findings there are TRACKED, not debt.

homes.tsv rows: `file<TAB>function<TAB>home<TAB>why`
  file      a repo-relative source path (a path that does not exist on this tree is skipped: a prototype's home, e.g.
            crates/gravity/fn/src/arena/raw.rs on the ledger2r branch, is listed before it lands)
  function  `*` = the whole file; `name` = every `fn name` in the file (its body, nested items included);
            `Type::name` = `fn name` inside an `impl .. Type` block; `<fn>@scope` = inside that fn, only the lexical extent
            of each guard `let g = ..cache_scope()` / `..cache_scope_for(owner, kind)` / `..enter_as(..)`: from the guard's line
            to the end of its block, or to `drop(g)` / `g.finish()` when the block ends it first (a cache scope that covers
            part of a function)
  home      H1 driver region / pool / spill (Charge) | H2 program arena | H3 mcx context | H4 cache holder or cache scope
            (`enter_as(Owner::Cache..)`, `memledger::arena::cache_scope()`, a holder Charge sized by the measured bytes)
            | H5 operator arena (the accounting prototype's OpArena / AVec / AMap)
  HC        (no row) a CHARGED line: `// alloc: <Program|Node|Exec|Cold> charged <owner expr>` (wave 2 R1 + R5; the bare
            `// alloc: charged <owner expr>` too), its bytes charged to that owner (ChargedVec, a Charge). memledger itself
            (ChargedVec, R1 (2)) is a home by name in crosscheck.py's HOME.
  why       the file:line that binds the owner
  COLD-TEXT (no row; not a home, not debt) a line whose trailing tag is a plain `// alloc: Cold` and whose detector item is
            text by name (format!, String, to_string, str::*, CString / OsString; cold_class() below): error, refusal and log
            text that leaves by unwinding or logging (refuse! / Reason, PgError, eprintln!, report lines). USER RULING via main
            2026-10-14T18:52:43Z: a NAMED class, counted in its own column, out of the debt total. A plain Cold tag on a
            non-text item (Vec, Box, a map, Arc, ...) stays debt and is counted MISTAGGED; on an item that may or may not be
            text by name (to_owned, clone, join, collect, into) it stays debt as COLD-UNPROVEN (write the text form).

The function spans come from the source (comments, strings and char literals blanked, braces matched), so the clippy leg
and the ledger-only checks need no IR. A function list, not a source marker: the homes are few, the files are hot files
other lanes edit, and a reviewer sees every new exemption in one place (ratchet.py base FAILs a homes.tsv that grows
without its HOMES-REVIEWED line moving with it)."""
import os
import re

RE_FN = re.compile(r"\bfn\s+([A-Za-z_]\w*)")
RE_IMPL = re.compile(r"\bimpl\b")


def blank(text, strings=True):
    """the source with comments, string / byte-string / raw-string literals and char literals replaced by spaces (newlines
    kept), so braces and `fn` tokens are code only; strings=False wipes the comments only (a name in a literal stays)"""
    out = list(text)
    n, i = len(text), 0

    def wipe_all(a, b):
        for k in range(a, min(b, n)):
            if out[k] != "\n":
                out[k] = " "

    def wipe(a, b):   # the literals' wipe
        if strings:
            wipe_all(a, b)
    while i < n:
        c = text[i]
        if c == "/" and i + 1 < n and text[i + 1] == "/":
            j = text.find("\n", i)
            j = n if j < 0 else j
            wipe_all(i, j)
            i = j
        elif c == "/" and i + 1 < n and text[i + 1] == "*":
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            wipe_all(i, j)
            i = j
        elif c in "rb" and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            m = re.match(r'(br|r|b)?(#*)"', text[i:i + 300]) if c in "rb" else None
            if m and ("r" in (m.group(1) or "")):
                hashes = m.group(2)
                end = text.find('"' + hashes, i + m.end())
                end = n if end < 0 else end + 1 + len(hashes)
                wipe(i, end)
                i = end
            elif m and m.group(1) == "b":
                i = skip_str(text, i + m.end(), wipe, i)
            elif text.startswith("b'", i):
                i = skip_char(text, i + 1, wipe, i)
            else:
                i += 1
        elif c == '"':
            i = skip_str(text, i + 1, wipe, i)
        elif c == "'":
            i = skip_char(text, i, wipe, i)
        else:
            i += 1
    return "".join(out)


def skip_str(text, j, wipe, start):
    n = len(text)
    while j < n:
        if text[j] == "\\":
            j += 2
            continue
        if text[j] == '"':
            j += 1
            break
        j += 1
    wipe(start, j)
    return j


def skip_char(text, i, wipe, start):
    """i at a quote: a char literal ('x', '\\n', '\\u{..}') is wiped; a lifetime ('a) is left"""
    n = len(text)
    if i + 1 < n and text[i + 1] == "\\":
        j = text.find("'", i + 3)
        j = n if j < 0 else j + 1
        wipe(start, j)
        return j
    if i + 2 < n and text[i + 2] == "'":
        wipe(start, i + 3)
        return i + 3
    return i + 1


# the guards: `cache_scope()`, `enter_as(..)`, and CACHE-CHARGE's `cache_scope_for(owner, kind)` (memlead; cvdict, cvpread),
# whose holder is named at the scope's open and charged as blocks are taken (main 2026-10-14T20:21:07Z)
RE_GUARD = re.compile(r"(?:let\s+(?:mut\s+)?(\w+)\s*=\s*)?[\w:]*(?:cache_scope\s*\(\s*\)|cache_scope_for\s*\(|enter_as\s*\()")


def fn_spans(text, guards=False):
    """[(name, impl self type or None, first line, last line)] of every fn with a body; with guards, a 5th item: the
    [(first line, last line)] lexical extents of the fn's cache-scope / enter_as guards"""
    b = blank(text)
    line_at = [0] * (len(b) + 1)
    ln = 1
    for k, ch in enumerate(b):
        line_at[k] = ln
        if ch == "\n":
            ln += 1
    line_at[len(b)] = ln
    # brace pairs
    match, st = {}, []
    for k, ch in enumerate(b):
        if ch == "{":
            st.append(k)
        elif ch == "}" and st:
            match[st.pop()] = k
    # impl blocks: `impl<..> [Trait for] Type<..> [where ..] {`
    impls = []
    for m in RE_IMPL.finditer(b):
        bol = b.rfind("\n", 0, m.start()) + 1
        if b[bol:m.start()].strip() not in ("", "unsafe", "default", "default unsafe"):
            continue          # `-> impl Trait`, `x: impl Fn()`: not an impl block
        j = b.find("{", m.end())
        if j < 0 or j not in match:
            continue
        head = b[m.end():j]
        if ";" in head:
            continue
        head = re.sub(r"\bwhere\b.*", "", head, flags=re.S)
        head = strip_generics(head).strip()
        if " for " in " " + head + " ":
            head = re.split(r"\bfor\b", head)[-1]
        ty = re.findall(r"[A-Za-z_]\w*", head)
        impls.append((j, match[j], ty[-1] if ty else None))
    out = []
    for m in RE_FN.finditer(b):
        k, depth = m.end(), 0
        body = None
        while k < len(b):
            ch = b[k]
            if ch in "(<[":
                depth += 1
            elif ch in ")>]":
                depth = max(0, depth - 1) if not (ch == ">" and b[k - 1] == "-") else depth
            elif ch == ";" and depth == 0:
                break
            elif ch == "{" and depth == 0:
                body = k
                break
            k += 1
        if body is None or body not in match:
            continue
        owner = None
        for lo, hi, ty in impls:
            if lo < m.start() < hi and (owner is None or lo > owner[0]):
                owner = (lo, ty)
        row = (m.group(1), owner[1] if owner else None, line_at[m.start()], line_at[match[body]])
        if guards:
            g = []
            for gm in RE_GUARD.finditer(b, body, match[body]):
                p, depth = gm.start(), 0
                while p > body:          # the innermost block holding the guard
                    p -= 1
                    if b[p] == "}":
                        depth += 1
                    elif b[p] == "{":
                        if depth == 0:
                            break
                        depth -= 1
                end = match.get(p, match[body])
                if gm.group(1):   # the guard ends early at `drop(g)` or at `g.finish(..)` (cache_scope_for's close)
                    v = re.escape(gm.group(1))
                    dm = re.search(r"\bdrop\s*\(\s*" + v + r"\s*\)|\b" + v + r"\s*\.\s*finish\s*\(", b[gm.end():end])
                    if dm:
                        end = gm.end() + dm.end()
                g.append((line_at[gm.start()], line_at[end]))
            row = row + (g,)
        out.append(row)
    return out


def strip_generics(s):
    out, depth = [], 0
    for i, ch in enumerate(s):
        if ch == "<":
            depth += 1
        elif ch == ">" and depth and not (i and s[i - 1] == "-"):
            depth -= 1
        elif depth == 0:
            out.append(ch)
    return "".join(out)


# a charged line (wave 2 rulings R1 + R5, main 2026-10-14T18:07:10Z / 18:16:35Z): `// alloc: <Program|Node|Exec|Cold> charged
# <owner expr>` (gravity_lint's grammar; the bare R1 form `// alloc: charged <owner expr>` too, on lines gravity_lint does not
# match) says the line's bytes are charged to that memledger owner (a ChargedVec, a hand-rolled Charge): TRACKED, home HC.
CHARGED = re.compile(r"//\s*alloc:\s*(?:(?:Program|Node|Exec|Cold)\s+)?charged\s+\S")

# D4505 (USER GO via main 2026-10-15T05:52:10Z): `charged caller` (the cvformat Q5 ruling, main 2026-10-14T19:53:37Z) says the
# line grows a buffer its CALLER hands in, so the caller's call site is the row. That holds only inside a pass-through writer the
# alloclint clippy.toml lists (a disallowed path: each call site is a finding in the caller's file). Anywhere else the tag charges
# nothing: the line is no home, and its findings are debt, counted MISTAGGED (caller_lines(); Homes.of / Homes.cold).
CALLER = re.compile(r"//\s*alloc:\s*(?:(?:Program|Node|Exec|Cold)\s+)?charged\s+caller\b")
CLIPPY = "crates/gravity/lint/alloclint/clippy.toml"
LISTED = re.compile(r'path\s*=\s*"([A-Za-z_]\w*(?:::[A-Za-z_]\w*)+)"\s*,\s*reason\s*=\s*"alloclint:')
NOT_WORKSPACE = {"std", "core", "alloc", "hashbrown", "smallvec", "bytes", "allocator_api2"}


def listed_writers(clippy_text):
    """{(crate, mods tuple, impl type or None, fn)}: the workspace fns the alloclint clippy.toml lists as disallowed paths (a
    `{ path = "<crate>::..::<fn>", reason = "alloclint:.." }` row; a capitalized segment before the fn is its impl type)"""
    out = set()
    for p in LISTED.findall(clippy_text):
        seg = p.split("::")
        if seg[0] in NOT_WORKSPACE or len(seg) < 2:
            continue
        ty = seg[-2] if len(seg) >= 3 and seg[-2][:1].isupper() else None
        mods = tuple(seg[1:-2] if ty else seg[1:-1])
        out.add((seg[0], mods, ty, seg[-1]))
    return out


def crate_mods(path):
    """(crate, mods tuple) of a source path `<crate dir>/src/<a>/<b>.rs` (crate = the dir's name, '-' -> '_'; lib.rs / main.rs
    are the root, mod.rs its directory), or None outside a crate's src/"""
    i = path.rfind("/src/")
    if i < 0 or not path.endswith(".rs"):
        return None
    crate = path[:i].rsplit("/", 1)[-1].replace("-", "_")
    mods = path[i + 5:-3].split("/")
    if mods[-1] in ("lib", "main", "mod"):
        mods = mods[:-1]
    return crate, tuple(mods)


def caller_lines(path, text, listed):
    """the line numbers of `path` (its source `text`) whose tag is `charged caller` (CALLER) OUTSIDE a listed pass-through
    writer: the innermost fn around the line is not in `listed` (listed_writers) for the file's crate and module"""
    lines = text.split("\n")
    hits = [n for n, ln in enumerate(lines, 1) if CALLER.search(ln)]
    if not hits:
        return []
    cm = crate_mods(path)
    spans = fn_spans(text)
    out = []
    for n in hits:
        inner = None
        for name, ty, lo, hi in spans:
            if lo <= n <= hi and (inner is None or hi - lo < inner[3] - inner[2]):
                inner = (name, ty, lo, hi)
        if cm is None or inner is None or (cm[0], cm[1], inner[1], inner[0]) not in listed:
            out.append(n)
    return out

# the COLD-TEXT class (USER RULING via main 2026-10-14T18:52:43Z): the line's LAST `// alloc:` tag (gravity_lint reads the
# last one, rsplit_once) is a plain `Cold`, nothing after it
COLD = re.compile(r"//\s*alloc:\s*Cold\s*$")
# a detector item (clippy's path, or an IR callee for ir-only rows) whose allocation is text by its name alone
TEXT_ITEM = re.compile(
    r"^(?:std|alloc)::(?:format|fmt::format)\b|^std::string::(?:String\b|ToString::)|^alloc::string::(?:String\b|ToString\b)"
    r"|^str::|^std::str::|alloc::str::<impl str>|^std::ffi::(?:CString|OsString)\b|^alloc::ffi::c_str::CString\b|CStr::to_string_lossy"
    r"|^core::fmt::Write::|<alloc::string::String as |<str as alloc::string::ToString>|<[^>]* as alloc::string::ToString>"
    r"|^alloc::fmt::format|^std::string::String::from_utf8_lossy"
    # an IR callee building an error's text (ir-only rows): PgError's constructors fill its message String; the error leaves
    # by unwinding (the COLD-TEXT ruling names PgError). D4611 / D4616: on a '// alloc: Cold' line they are COLD-TEXT
    r"|^types_error::pg_error::PgError::(?:new|error|warning|notice|internal)$")
# an item that is text for some receivers and not for others (`s.to_owned()` on a &str is a String, on a slice a Vec)
MAYBE_TEXT_ITEM = re.compile(
    r"ToOwned::to_owned|Clone::clone|slice::(?:join|concat)$|Cow::into_owned|Into(?:<U>)?>?::into$|From(?:<T>)?>?::from$"
    r"|Iterator::collect$|Extend::extend$|ToOwned>::to_owned|<T as core::convert::Into")


def text_item(item):
    """'text' (the allocation is text by its name), 'maybe' (text for some receivers only) or 'non-text'"""
    if TEXT_ITEM.search(item):
        return "text"
    if MAYBE_TEXT_ITEM.search(item):
        return "maybe"
    return "non-text"


# a Box of an error value on a Cold line: the boxed error / refusal that leaves by unwinding (main 2026-10-14T20:21:07Z:
# text_hosts.rs's `TableBuildError::Pg(Box::new(PgError::error(..)..))`, the 53200 path) is the error text's carrier.
# main 2026-10-15T08:00:43Z: a Box of an error held in a variable (`Box::new(e)`) names its type by the turbofish,
# `Box::<PgError>::new(e)` (or Reason / an *Error type): the compiler proves the name, so the line is COLD-TEXT too.
BOX_ITEM = re.compile(r"(?:^|::)Box(?:::new)?$")
ERROR_VALUE = re.compile(r"Box::new\s*\(\s*[\w:]*(?:PgError|Reason|[A-Z]\w*Error)\b"
                         r"|Box::<\s*[\w:]*(?:PgError|Reason|[A-Z]\w*Error)\s*>::new\s*\(|\brefuse!")


def cold_class(line_text, item):
    """None (the line has no plain Cold tag), COLD-TEXT, COLD-UNPROVEN or MISTAGGED"""
    if not COLD.search(line_text.rstrip()) or CHARGED.search(line_text):
        return None
    if BOX_ITEM.search(item) and ERROR_VALUE.search(line_text):
        return "COLD-TEXT"
    return {"text": "COLD-TEXT", "maybe": "COLD-UNPROVEN", "non-text": "MISTAGGED"}[text_item(item)]


class Homes:
    def __init__(self, root, path):
        self.rows, self.whole, self.spans, self.missing, self.unmatched = [], {}, {}, [], []
        self.root, self.text = root, {}
        self.callers, self.listed = {}, None   # D4505: file -> its unlisted `charged caller` lines; clippy.toml's writers
        if not path or not os.path.exists(path):
            return
        for ln in open(path):
            if ln.startswith("#") or not ln.strip():
                continue
            c = ln.rstrip("\n").split("\t")
            if len(c) < 3:
                continue
            f, fn, home = c[0], c[1], c[2]
            self.rows.append((f, fn, home, c[3] if len(c) > 3 else ""))
            full = os.path.join(root, f)
            if not os.path.exists(full):
                self.missing.append((f, fn, home))
                continue
            if fn == "*":
                self.whole[f] = home
                continue
            if f not in self.spans:
                self.spans[f] = []
            base, _, sc = fn.partition("@")
            ty, _, name = base.rpartition("::")
            hit = False
            for name_, owner, lo, hi, gs in fn_spans(open(full, errors="replace").read(), guards=True):
                if name_ == name and (not ty or owner == ty):
                    if sc == "scope":
                        for glo, ghi in gs:
                            self.spans[f].append((glo, ghi, home, fn))
                            hit = True
                    else:
                        self.spans[f].append((lo, hi, home, fn))
                        hit = True
            if not hit:
                self.unmatched.append((f, fn, home))

    def of(self, f, line):
        """the home a source line runs in, or HC for a charged line (None: not a home)"""
        if f in self.whole:
            return self.whole[f]
        for lo, hi, home, _ in self.spans.get(f, ()):
            if lo <= line <= hi:
                return home
        if self.charged(f, line):
            return "HC"
        return None

    def charged(self, f, line):
        """the source line carries the charged tag (CHARGED), and a `charged caller` tag sits in a listed pass-through writer
        (D4505: elsewhere it charges nothing)"""
        if f not in self.text:
            full = os.path.join(self.root, f)
            self.text[f] = open(full, errors="replace").read().split("\n") if f and os.path.isfile(full) else []
        t = self.text[f]
        return 0 < line <= len(t) and CHARGED.search(t[line - 1]) is not None and not self.caller_unlisted(f, line)

    def caller_unlisted(self, f, line):
        """D4505: the line's tag is `charged caller` outside a pass-through writer the alloclint clippy.toml lists"""
        if f not in self.callers:
            if self.listed is None:
                p = os.path.join(self.root, CLIPPY)
                self.listed = listed_writers(open(p).read()) if os.path.exists(p) else set()
            full = os.path.join(self.root, f)
            text = open(full, errors="replace").read() if f and os.path.isfile(full) else ""
            self.callers[f] = set(caller_lines(f, text, self.listed))
        return line in self.callers[f]

    def line(self, f, line):
        """the source line's text ('' when the file or line is absent)"""
        self.charged(f, line)
        t = self.text[f]
        return t[line - 1] if 0 < line <= len(t) else ""

    def cold(self, f, line, item):
        """the line's COLD-TEXT class for this item (cold_class), None when the line has no plain Cold tag; MISTAGGED for a
        `charged caller` tag outside a listed pass-through writer (D4505: debt, counted in the mistagged column)"""
        if self.caller_unlisted(f, line):
            return "MISTAGGED"
        return cold_class(self.line(f, line), item)

    def fn_of(self, f, line):
        if f in self.whole:
            return "*"
        for lo, hi, _, fn in self.spans.get(f, ()):
            if lo <= line <= hi:
                return fn
        if self.charged(f, line):
            return "// alloc: charged"
        return None
