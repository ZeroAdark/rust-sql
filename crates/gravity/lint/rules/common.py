"""The v5 rules' shared pieces (PLAN-v5 §3.2): the source model, the roles (homes, families, LEGACY) and helpers.

Python 3, standard library only. It reads the checkout under test as text and never imports from it, so a stale build
cannot answer for another tree (D793). Paths are repo-relative with '/' separators.
"""
import bisect
import fnmatch
import os
import re

G = "crates/gravity/"
# The planner's Gravity passes: R3 and R4 cover them (path100 r3wide2.py's scope); nothing else does.
PLANNER = "crates/backend/optimizer/plan/planner/src/"
PLANNER_FILES = ("agg_algebra.rs", "gravity_paths.rs", "flatten_group.rs", "groupkey_trim.rs", "decorrelate.rs",
                 "groupingsets_union.rs")
GENERATED_DIRS = (G + "fn/src/shims/",)          # written by fn's generator; a header check catches the rest
NOT_SCANNED = (G + "lint/",)                      # the checks themselves (gravity_lint's compiled tests, these rules)
LOWERING = ("lowerv4", "lower")                   # one crate, `lower`, after the end merge (DECISIONS 15)
SUBSTRATE = ("kernels", "expr", "abi", "program", "parallel", "dict")


def ledger_join(path, entries):
    """A debt ledger's rows (lint/EFFECTS-DEBT, KEYSEM-DEBT, LIFETIME-DEBT; shared lists are directories): `entries` is
    [(file name, text)] of the ledger directory `path` -> (text, at): the rows one per line in file-name order (the README
    and dot files skipped), and at(n) = (row file, line in it) for line n of the text."""
    lines, where = [], []
    for name, t in sorted(entries):
        if name == "README" or name.startswith("."):
            continue
        for k, line in enumerate(t.split("\n"), 1):
            if line.strip():
                lines.append(line)
                where.append((path + "/" + name, k))
    return "\n".join(lines) + ("\n" if lines else ""), lambda n: where[n - 1] if 0 < n <= len(where) else (path, n)


def ledger_file(path, text):
    """A ledger still in one file (a parent before LEDGER-DIRS): (text, at) with at(n) = (path, n)."""
    return text, lambda n: (path, n)


def ledger_loc(at, n):
    """'file:line' of ledger line n."""
    p, k = at(n)
    return f"{p}:{k}"


def L(rest):
    """A path inside the lowering crate, in both of its names."""
    return [G + c + "/src/" + rest for c in LOWERING]


def in_any(path, globs):
    """`path` is inside one of `globs`: 'dir/' is a prefix, a name with * ? [ is fnmatch, anything else exact."""
    for g in globs:
        if g.endswith("/"):
            if path.startswith(g):
                return True
        elif any(c in g for c in "*?["):
            if fnmatch.fnmatchcase(path, g):
                return True
        elif path == g:
            return True
    return False


def crate(path):
    return path[len(G):].split("/", 1)[0] if path.startswith(G) else None


def crate_dir(path):
    """The crate a source file is in, nested crates included: `backend/tpde` for crates/gravity/backend/tpde/src/x.rs."""
    if not path.startswith(G) or "/src/" not in path:
        return None
    return path[len(G):path.index("/src/")]


def is_crate_root(path):
    d = crate_dir(path)
    return d is not None and path[len(G) + len(d):] in ("/src/lib.rs", "/src/main.rs")


def family(path):
    """The PG node family a source file belongs to (`<crate>/src/ops/<family>/...` or `ops/<family>.rs`), else None.
    A split lane's subdirectory (`ops/agg/trans/`) is its node's family."""
    m = re.match(re.escape(G) + r"[^/]+/src/ops/([a-z0-9_]+)(/|\.rs$)", path)
    return m.group(1) if m and m.group(1) != "mod" else None


def is_lowering(path):
    return crate(path) in LOWERING


def in_src(path):
    """Production source: `crates/gravity/<crate>/src/**.rs`."""
    return path.startswith(G) and "/src/" in path and path.endswith(".rs")


def in_planner(path):
    return path.startswith(PLANNER) and path[len(PLANNER):] in PLANNER_FILES


# ---------------------------------------------------------------------------------------------------------------------
# Source model: code lines with comments removed and string bodies blanked; comment text per line; #[cfg(test)] items
# blanked in both. Line numbers are kept.

# One token pattern: a line comment, a (non-nested) block comment, a raw string, a string, a char literal. Each
# alternative starts with its own first character, so the scan runs at the regex engine's prefix speed; an identifier
# cannot end right before a quote in Rust 2021 (reserved prefixes), so no look-behind is needed.
_TOK = re.compile(r"/(?:/[^\n]*|\*(?:[^*]|\*(?!/))*\*/)|b?(?:r(#*)\".*?\"\1|\"(?:\\.|[^\"\\])*\""
                  r"|'(?:\\u\{[0-9a-fA-F]+\}|\\x[0-9a-fA-F]{2}|\\.|[^\\'\n])')|r(#*)\".*?\"\2", re.S)
_NESTED = re.compile(r"/\*(?:[^*]|\*(?!/))*?/\*")
_CFG_TEST = re.compile(r"#\[cfg\(test\)\]")
_NL = re.compile("\n")
_NOT_NL = re.compile(r"[^\n]")
_BRACE = re.compile(r"[{};]")


def lex(text):
    """-> (code, comments): two lists of len(lines). Code keeps `""` for strings, chars as written (a bracket char
    becomes ' '), and drops comments; comments keep each line's comment text. #[cfg(test)] items are blanked in both."""
    found = []

    def repl(m):
        t = m.group()
        c = t[0]
        if c == "/":
            found.append((m.start(), t))
            return "\n" * t.count("\n")
        if c == "'" or t[:2] == "b'":
            return "' '" if any(b in t for b in "{}()[]") else t
        return '""' + "\n" * t.count("\n")

    if _NESTED.search(text):          # a nested block comment: rare; the simple pattern would end it early
        text = _unnest(text)
    code = _TOK.sub(repl, text)
    lines = code.split("\n")
    comments = [""] * len(lines)
    if found:
        nl = [m.start() for m in _NL.finditer(text)]
        for off, t in found:
            first = bisect.bisect_left(nl, off)
            for n, part in enumerate(t.split("\n")):
                comments[first + n] += part[2:] if n == 0 and t.startswith("//") else part
    if "#[cfg(test)]" in code:
        for t in reversed(list(_CFG_TEST.finditer(code))):
            i, depth = len(code), 0
            for b in _BRACE.finditer(code, t.end()):
                c = b.group()
                if c == "{":
                    depth += 1
                elif c == "}":
                    depth -= 1
                    if depth == 0:
                        i = b.end()
                        break
                elif depth == 0:            # `;` ends a body-less item (`mod tests;`, a `use`)
                    i = b.end()
                    break
            a, b = code.count("\n", 0, t.start()), code.count("\n", 0, i)
            for n in range(a, b + 1):
                comments[n] = ""
            code = code[:t.start()] + _NOT_NL.sub(" ", code[t.start():i]) + code[i:]
        lines = code.split("\n")
    return lines, comments


def _unnest(text):
    """Flatten nested block comments to one level (inner /* and */ become spaces), keeping offsets."""
    out, i, depth = [], 0, 0
    for m in re.finditer(r"/\*|\*/", text):
        if m.group() == "/*":
            depth += 1
            if depth > 1:
                out.append(text[i:m.start()] + "  ")
                i = m.end()
        else:
            if depth > 1:
                out.append(text[i:m.start()] + "  ")
                i = m.end()
            depth = max(0, depth - 1)
    out.append(text[i:])
    return "".join(out)


def generated(text):
    head = text.split("\n", 5)[:5]
    return any("@generated" in l or "generated by" in l for l in head)


class Src:
    """One production source file."""
    __slots__ = ("path", "text", "lines", "code", "comments", "family", "crate", "_joined")

    def __init__(self, path, text):
        self.path, self.text = path, text
        self.lines = text.split("\n")
        self.code, self.comments = lex(text)
        self.family, self.crate = family(path), crate(path)
        self._joined = {}

    def joined(self, which="code"):
        """The code (or comment) lines as one text."""
        if which not in self._joined:
            self._joined[which] = "\n".join(self.code if which == "code" else self.comments)
        return self._joined[which]

    def hits(self, rx, which="code", keep=None):
        """The 1-based lines where `rx` matches (one search over the whole text, not one per line); `keep(text, m)`
        filters matches (a word-boundary test is faster here than a leading \\b in the pattern)."""
        t = self.joined(which)
        out, line, at = [], 1, 0
        for m in rx.finditer(t):
            if keep and not keep(t, m):
                continue
            line += t.count("\n", at, m.start())
            at = m.start()
            if not out or out[-1] != line:
                out.append(line)
        return out


def word_start(t, m):
    """The match starts a word (the character before it is not a word character)."""
    i = m.start()
    return i == 0 or not (t[i - 1].isalnum() or t[i - 1] == "_")


def walk_sources(root):
    """Every production .rs file the rules read: crates/gravity/*/src/** and the planner's Gravity files."""
    out = []
    groot = os.path.join(root, G)
    crates = []
    if os.path.isdir(groot):
        for dp, dn, fn in os.walk(groot):       # top-level crates, and nested ones (backend/tpde) by their Cargo.toml
            dn[:] = sorted(d for d in dn if d not in ("src", "tests", "benches", "examples", "target")
                           and not d.startswith("."))
            if os.path.isdir(os.path.join(dp, "src")) and (os.path.dirname(dp.rstrip(os.sep)) ==
                                                            groot.rstrip(os.sep) or "Cargo.toml" in fn):
                crates.append(dp)
        for c in sorted(crates):
            src = os.path.join(c, "src")
            for dp, dn, fn in os.walk(src):
                dn[:] = sorted(d for d in dn if d != "target")
                for f in sorted(fn):
                    if f.endswith(".rs"):
                        out.append(os.path.relpath(os.path.join(dp, f), root).replace(os.sep, "/"))
    for f in PLANNER_FILES:
        if os.path.isfile(os.path.join(root, PLANNER, f)):
            out.append(PLANNER + f)
    return [p for p in out if not in_any(p, GENERATED_DIRS + NOT_SCANNED)]


def families(root):
    """The family names present in the tree: every entry under any crates/gravity/*/src/ops/ (dirs, and files but mod.rs)."""
    out, groot = set(), os.path.join(root, G)
    if os.path.isdir(groot):
        for c in os.listdir(groot):
            ops = os.path.join(groot, c, "src", "ops")
            if os.path.isdir(ops):
                for e in os.listdir(ops):
                    if e != "mod.rs":
                        out.add(e[:-3] if e.endswith(".rs") else e)
    return out


def read(root, path):
    with open(os.path.join(root, path), encoding="utf-8", errors="replace") as fh:
        return fh.read()


# ---------------------------------------------------------------------------------------------------------------------
# Homes (PLAN-v5 §2.4; FILE-MAP.tsv targets). A home is a module: a directory ('/'), a file, or a glob of either.

HOME = {
    # effects/visible.rs: the visible half (user 10-03); fn/src/effects.rs and exec/src/effects.rs: the two lower layers,
    # below crates the home cannot reach (closed at these two: a third is a design change)
    "effects": [G + "decompose/src/effects/"] + L("effects.rs") + L("effects/") + [G + "fn/src/effects.rs", G + "exec/src/effects.rs"],
    "typesem": [G + "fn/src/typesem/", G + "kernels/src/typesem/"] + L("rep/"),
    "numeric": [G + "fn/src/typesem/numeric.rs", G + "fn/src/typesem/numeric/"] + L("ops/agg/numeric/"),
    "facts": [G + "decompose/src/facts/"],
    "lifetime": [G + "decompose/src/lifetime/", G + "program/src/lifetime.rs"] + L("lifetime/"),   # keep table + declared scopes; program/src/lifetime.rs: the run-again declarations lowerv4's half writes into the program
    "catalog": [G + "decompose/src/catalog/"],
    "cost": [G + "program/src/cost/"],
    "inputs": L("inputs/") + [G + "exec/src/inputs/", G + "hosts/src/worker_arm.rs", G + "hosts/src/inputs.rs", G + "fn/src/inputs.rs", G + "driver/src/env.rs"],   # worker_arm: the worker carry (tree.md); hosts/src/inputs.rs: the hosts' run-time readers (hosts cannot reach exec's inputs); fn/src/inputs.rs: gravity_fn's own (fn sits below every other inputs home); driver::env: the env reader below exec
    "contract": [G + "kernels/src/contract.rs", G + "kernels/src/host_abi.rs", G + "abi/src/word.rs"] + L("frame/host.rs"),
    "layout": [G + "abi/src/"],                                   # contract::layout: the repr(C) blocks
    "mem": [G + "driver/src/mem/", G + "fn/src/session.rs", G + "abi/src/ctx.rs"],
    "iface": L("frame/iface.rs") + L("frame/iface/"),   # frame/iface/<topic>.rs: the home's sibling files, re-exported by iface.rs
    "admit": L("admit/") + [G + "decompose/src/admit.rs", G + "exec/src/admit/"],
    "entry": [G + "exec/src/admit/entry/", G + "entry/src/"],      # entry folds into exec/src/admit/entry (admit lane)
    "rerun": [G + "decompose/src/rerun.rs", G + "exec/src/rerun/", G + "kernels/src/rerun.rs"] + L("rerun/"),
    "normalize": [G + "decompose/src/normalize/"],
    "keyhash": [G + "abi/src/hash.rs", G + "expr/src/emit/hash.rs"],
    "table": L("table/") + [G + "kernels/src/table/", G + "exec/src/table/"],
    "dict": [G + "dict/src/", G + "kernels/src/dict/", G + "hosts/src/dict/"] + L("dict/"),
    # each module's raw.rs or raw/ under src: a family's (ops/<family>/raw.rs) and any other module's that needs unsafe
    # (driver, fn, exec/src/runner ...; main's ruling 2026-10-05T00:14:36Z, glob widened 00:49:23Z)
    "raw": [G + "*/src/raw.rs", G + "*/src/raw/*", G + "*/src/*/raw.rs", G + "*/src/*/raw/*"],
    # The substrate crates' own modules outside ops/ (PLAN-v5-constructs #10, revision 3 gap G2; dict joined by main's
    # decision, DECISIONS.md "Main decides these")
    "substrate": [G + c + "/src/" for c in SUBSTRATE],
}


def home(*names):
    out = []
    for n in names:
        out += HOME[n]
    return out


def is_substrate(path):
    """A substrate crate's own module (SUBSTRATE), outside ops/: a family's code in a substrate crate follows the
    family rule (PLAN-v5-constructs #10, gap G2)."""
    return in_any(path, HOME["substrate"]) and family(path) is None


# ---------------------------------------------------------------------------------------------------------------------
# Small helpers for rule files

def regex_hits(lines, rx):
    return [i + 1 for i, l in enumerate(lines) if rx.search(l)]


def words(names):
    return r"\b(?:" + "|".join(re.escape(n) for n in names) + r")\b"


_PATH_PREFIX = re.compile(r"\b(?:crate|super|self|gravity_[a-z0-9_]+)::")
_VIS = re.compile(r"\bpub(?:\([^)]*\))?\s+")


def normalize_line(s):
    """A line as a move may leave it (moveonly.py's normalization, per line): no comments, no visibility, no path
    prefixes, no whitespace."""
    s = s.split("//", 1)[0]
    s = _VIS.sub("", s)
    s = _PATH_PREFIX.sub("", s)
    return re.sub(r"\s+", "", s)
