"""RT: a thread-local outside the thread-state registry (C7 (b), reports/BUG-RECURRENCE-20261011.md §3 C7). Per-thread
state is how a pool claim read the wrong session or database (D3093 catcache registration, D3532 the locale cache, D3542
whole-row type OIDs, D3576 toast access, D3522 an InitPlan parameter, D3425 a BufFile owner): each was a thread-local
nobody had asked "who owns it, when does it go back, what does a claim on another thread see". The registry answers the
three for every thread-local a claim can run into, in one place: lint/THREAD-STATE/ (a directory, one file per source
file, named like the ledgers: <path under crates/, '/' as '.'>), one row per static:

    <path under crates/>  <static>  <owner>  <reset>  <carry>  [<member | note>]

- owner: a tag of lint/THREAD-STATE/OWNERS (tag, what it covers, the lane that answers today).
- reset: when the value goes back. scope:<fn> (set for an extent; that fn's guard or its own code puts it back),
  statement:<fn>, xact:<fn>, at:<fn> (an event: an invalidation callback, DISCARD, an error cleanup, a pool's trim), each
  naming a function of the static's file (or <fn>@<path under crates/>) whose body names the static or a static of the same
  thread_local! block, or a type (a capitalized name) whose `impl Drop` does; claim (armed per claim and put back at its
  end by the CLAIM_REGISTRY member the last field names); key:<what> (never reset: every read checks its key or stamp);
  call (scratch: each use takes or overwrites it); exit (lives as long as its thread: a client backend's thread is its
  session's, a retained bgworker standby's spans its tasks (launch_backend, init_small wretain), a pool thread's is the
  process's); test.
- carry: what a pool claim on another thread sees (the claim census's classes, unchanged): claim (its leader's, through
  the member named in the last field: hosts worker_arm.rs claim_census_is_pinned checks the member carries it), leader
  (read only on the backend's own statement path), cache (keyed or stamped by every input it depends on), scratch,
  thread (the thread's own machinery, or the outcome of its own run), test, debt (a claim may read it and nothing carries
  it yet: CARRY-DEBT, which only shrinks).
- They agree: claim <-> claim, test <-> test, scratch <-> call, cache -> key:, exit -> leader or thread.

Scope (C7 coverage E2): every source under crates/ outside tests/ (a claim's path reaches nearly every crate, so no
prefix list is the crates a claim runs). A static of the scope with no row FAILS (a new thread_local!), as do a row whose
static is gone, two rows for one static, a row outside its source file's registry file, an owner not in OWNERS and a
reset or carry that does not hold. A static not yet registered is a row of lint/TLS-DEBT/ (one file per source file):
<path> <static> <owner lane> <carry today> <what is missing>, or, one the scope gained at E2, of lint/SCOPE-DEBT/:
<path> <static> <owner lane> <what is missing>. Both only shrink: on a landing (--base) a debt row the parent lacks
fails, as does a carry=debt registry row the parent lacks (CARRY-DEBT); a registered static's debt row goes; a SCOPE-DEBT
row whose static is gone is spent (counted, as in LEADER-TLS-DEBT below).

Runtime half: a crates/gravity static whose carry is leader is declared through leader_tls! (crates/_support/leader_tls:
the thread_local! block inside `leader_tls::leader_tls! {`; a debug build raises ERROR XX000 at any access inside a
pool claim, a release build is the plain block) or is a row of lint/LEADER-TLS-DEBT/ (<path> <static> <owner> <note>),
which only shrinks (a row whose static and registry row are gone is spent: counted in the note, not a failure);
leader_tls! declares leader or test statics only.

Macro-declared thread-locals (C7 coverage E1): a macro_rules! of the scope whose thread_local! block declares `static $x`
declares one static per invocation (or per entry of a matcher that opens with a repetition `$($x:ident, ..;)+`), in the
invoking file, named by the identifier at $x's place (vfd.rs / init_small's scalar_global!, guc_tables' session_guc_*!);
its template `$x` is no static. The invocation's other identifiers (its accessors) count as names of the static's block
for its reset. A macro whose body hands its own `$y` to such a macro's static place declares through it, one static per
invocation as well (vacuum lib.rs vacuum_guc_int! -> guc_tables session_guc_int!). A `static $x` RT cannot read so (several rules; a fragment before $x that is no ident) FAILS.

Clause (d), unwind-safe thread scopes (D4112, D4113): a scope:<X> reset puts the static back on any exit, an ERROR that
unwinds included: X is a type with an `impl Drop` or a function whose body holds such a type of its file whose drop puts
the static back. A bare begin / end pair FAILS, or is a row of lint/GUARD-DEBT/ (<path> <static> <owner lane> <what is
missing>; only shrinks; a row whose reset became a guard goes). A row's note `rig: f, g` names the bare pair kept for the
in-process test rigs: a call of f or g outside cfg(test) code and tests/ FAILS.

Clause (e), state kept across statements (D4114): a static that outlives its statement (reset exit or at:<fn>) and grows
(its declared type holds a Vec / map / set / String) states its bound in its note: a cap ("cap", "at most", "bounded"),
"per session by design" (PostgreSQL's own session caches) or "test mode"; else it FAILS. A note that calls the static
"per-run data" or "per-statement data" with such a reset FAILS: the data moves into the run, or resets at statement: /
scope:."""
import os
import re
import subprocess

from common import G, ledger_loc
from rl_lifetime import _close, functions
from common import Src

ID = "RT"
TITLE = "a thread-local outside the thread-state registry"
INSTEAD = "a row in lint/THREAD-STATE/<file> naming its owner, reset point and carry rule"
GLOBAL = True
REGISTRY = G + "lint/THREAD-STATE"
OWNERS = REGISTRY + "/OWNERS"
DEBT = G + "lint/TLS-DEBT"
# The runtime half (main 2026-10-12T00:17:07Z): a leader-class static of LEADER_SCOPE is declared through leader_tls!
# (crates/_support/leader_tls), whose debug form raises on a touch inside a pool claim; one not yet is a row of
# LEADER-TLS-DEBT, which only shrinks.
LEADER_DEBT = G + "lint/LEADER-TLS-DEBT"
LEADER_SCOPE = ("gravity/",)
# the block's `thread_local!` line follows `leader_tls! {` (any path before it) with nothing but whitespace between
_WRAP = re.compile(rb"leader_tls!\s*\{\s*$")
SCOPE_DEBT = G + "lint/SCOPE-DEBT"
# C7 clause (d), unwind-safe thread scopes (D4112, D4113): an extent's reset is a Drop guard; the bare pairs not yet one
GUARD_DEBT = G + "lint/GUARD-DEBT"
_RIG = re.compile(r"\brig: *([A-Za-z_][A-Za-z0-9_]*(?: *, *[A-Za-z_][A-Za-z0-9_]*)*)")
CARRY = ("claim", "leader", "cache", "scratch", "thread", "test", "debt")
_GROWS = re.compile(r"\b(?:Vec|VecDeque|HashMap|HashSet|BTreeMap|BTreeSet|IndexMap|IndexSet|String|PgVec|SmallVec|"
                    r"LinkedList|BinaryHeap)\b")
_BOUND = re.compile(r"\b(?:cap|capped|caps|at most|bounded|per session by design|test mode)\b", re.I)
_PER_RUN = re.compile(r"\bper-(?:run|statement) data\b", re.I)
FN_RESETS = ("scope", "statement", "xact", "at")
BARE_RESETS = ("claim", "call", "exit", "test")
_IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*$")

STARTS = (b"thread_local!", b"std::thread_local!", b"::std::thread_local!")
_WS = b" \t\n\x0c\r"
_OPEN, _CLOSE = b"({[", b")}]"


def tls_blocks(src, starts=None):
    """-> [[static, ..] per thread_local! block] of src (bytes): the session test's tls_names() parser (session tests.rs,
    tools tls_census.py), byte for byte, with the names kept per block (and each block's line start appended to
    `starts`)."""
    blocks, resume, start, n = [], 0, 0, len(src)
    while start < n:
        end = src.find(b"\n", start)
        end = n if end < 0 else end
        if start >= resume and src[start:end].lstrip().startswith(STARTS):
            names = []
            j = src.index(b"!", start) + 1
            while j < n and src[j] not in _OPEN:
                j += 1
            depth = 0
            while j < n:
                c = src[j]
                if src.startswith(b"//", j):
                    k = src.find(b"\n", j)
                    j = n if k < 0 else k
                    continue
                if c == 0x22:   # "
                    j += 1
                    while j < n and src[j] != 0x22:
                        j += 2 if src[j] == 0x5C else 1
                    j += 1
                    continue
                if c == 0x27 and j + 2 < n and src[j + 2] == 0x27:   # 'c'
                    j += 3
                    continue
                if c in _OPEN:
                    depth += 1
                elif c in _CLOSE:
                    depth -= 1
                    if depth == 0:
                        break
                elif depth == 1 and src.startswith(b"static", j) and (
                        j == 0 or not (chr(src[j - 1]).isascii() and (chr(src[j - 1]).isalnum() or src[j - 1] in b"_'"))):
                    k = ws = j + 6
                    while k < n and src[k] in _WS:
                        k += 1
                    i = k
                    if k > ws and k < n and (chr(src[k]).isascii() and (chr(src[k]).isalpha() or src[k] in b"_$")):
                        k += 1
                        while k < n and chr(src[k]).isascii() and (chr(src[k]).isalnum() or src[k] == 0x5F):
                            k += 1
                        name = src[i:k].decode()
                        while k < n and src[k] in _WS:
                            k += 1
                        if k < n and src[k] == 0x3A:   # :
                            names.append(name)
                            j = k + 1
                            continue
                j += 1
            blocks.append(names)
            if starts is not None:
                starts.append(start)
            resume = j
        start = end + 1
    return blocks


def in_scope(p):
    """`p` (a path under crates/) is in the registry's scope: outside tests/."""
    return "/tests/" not in p


# Every call names its repository by `-C root` with git's hook variables out (run.py GIT_ENV: a hook's GIT_DIR / GIT_INDEX_FILE
# would point the call at another repository or index).
_GIT_ENV = {k: v for k, v in os.environ.items() if k not in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_PREFIX")}
_FILES = {}


def scope_files(root):
    """-> {path under crates/: bytes} of the scope's .rs files. In a git checkout: the index's files (a file the sparse
    checkout leaves out of the working tree, flag S, is read from the index: check.sh's Mac preflight worktree holds
    crates/gravity only) plus untracked ones, each read from the working tree when it is there; a tracked file deleted
    from the working tree is gone. Outside git (the selftest's scratch trees): the working tree."""
    if root in _FILES:
        return _FILES[root]
    out = {}
    r = subprocess.run(["git", "-C", root, "ls-files", "-z", "-t", "--cached", "--others", "--exclude-standard", "--", "crates/"],
                       capture_output=True, env=_GIT_ENV)
    if r.returncode == 0 and subprocess.run(["git", "-C", root, "rev-parse", "--show-toplevel"], capture_output=True,
                                            env=_GIT_ENV).stdout.decode().strip() == os.path.realpath(root):
        sparse = []
        for item in r.stdout.decode("utf-8", "surrogateescape").split("\0"):
            if len(item) < 3 or not item.endswith(".rs"):
                continue
            tag, path = item[0], item[2:]
            p = path[len("crates/"):]
            if not in_scope(p):
                continue
            full = os.path.join(root, path)
            if tag == "S" and not os.path.isfile(full):
                sparse.append(path)
            elif os.path.isfile(full):
                with open(full, "rb") as fh:
                    out[p] = fh.read()
        if sparse:
            cat = subprocess.run(["git", "-C", root, "cat-file", "--batch"], input="".join(f":{x}\n" for x in sparse).encode(),
                                 capture_output=True, env=_GIT_ENV)
            data, i = cat.stdout, 0
            for path in sparse:
                j = data.index(b"\n", i)
                head = data[i:j].split()
                if len(head) < 3 or head[1] != b"blob":
                    i = j + 1
                    continue
                size = int(head[2])
                out[path[len("crates/"):]] = data[j + 1:j + 1 + size]
                i = j + 1 + size + 1
    else:
        for dp, dn, fn in os.walk(os.path.join(root, "crates")):
            dn[:] = sorted(d for d in dn if d != "target" and not d.startswith("."))
            for f in sorted(fn):
                full = os.path.join(dp, f)
                p = os.path.relpath(full, os.path.join(root, "crates")).replace(os.sep, "/")
                if f.endswith(".rs") and in_scope(p):
                    with open(full, "rb") as fh:
                        out[p] = fh.read()
    _FILES[root] = out
    return out


_MACRO = re.compile(rb"macro_rules!\s*([A-Za-z_][A-Za-z0-9_]*)\s*\{")
_FRAG = re.compile(rb"\$([A-Za-z_][A-Za-z0-9_]*)\s*:\s*([a-z_]+)")
_IDENT_B = re.compile(rb"(?:#\[[^\]]*\]\s*)*([A-Za-z_][A-Za-z0-9_]*)")


def _skip(src, j):
    """-> the index past a comment or a string literal starting at src[j], or j."""
    if src.startswith(b"//", j):
        k = src.find(b"\n", j)
        return len(src) if k < 0 else k
    if src.startswith(b"/*", j):
        k = src.find(b"*/", j + 2)
        return len(src) if k < 0 else k + 2
    if src[j] == 0x22:
        j += 1
        while j < len(src) and src[j] != 0x22:
            j += 2 if src[j] == 0x5C else 1
        return j + 1
    return j


def _split(body, sep):
    """-> the parts of `body` (bytes) split at `sep` (a byte) outside brackets, comments and strings."""
    parts, depth, start, j = [], 0, 0, 0
    while j < len(body):
        k = _skip(body, j)
        if k != j:
            j = k
            continue
        c = body[j]
        if c in _OPEN:
            depth += 1
        elif c in _CLOSE:
            depth -= 1
        elif c == sep and depth == 0:
            parts.append(body[start:j])
            start = j + 1
        j += 1
    parts.append(body[start:])
    return parts


def _bclose(src, i):
    """-> the index of the bracket closing src[i] (comments and strings skipped), or -1."""
    depth, j = 0, i
    while j < len(src):
        k = _skip(src, j)
        if k != j:
            j = k
            continue
        if src[j] in _OPEN:
            depth += 1
        elif src[j] in _CLOSE:
            depth -= 1
            if depth == 0:
                return j
        j += 1
    return -1


_TOKEN = re.compile(rb"\$[A-Za-z_][A-Za-z0-9_]*(?:\s*:\s*[a-z_]+(?![A-Za-z0-9_]))?|[A-Za-z_][A-Za-z0-9_]*|#\[[^\]]*\]|\S")


def _tokens(b):
    """-> the tokens of `b` (bytes) up to its first bracket group: identifiers (a `$x:kind` fragment one token) and single
    punctuation characters; comments and attributes skipped."""
    out, j = [], 0
    while j < len(b):
        k = _skip(b, j)
        if k != j:
            j = k
            continue
        m = _TOKEN.match(b, j)
        if m is None:
            j += 1
            continue
        t = m.group(0)
        if t[:1] in b"([{" or t.startswith(b"$("):
            break
        if not t.startswith(b"#["):
            out.append(re.sub(rb"\s+", b"", t))
        j = m.end()
    return out


_PRIMS = {"true", "false", "bool", "char", "str", "usize", "isize", "f32", "f64", "None", "Some", "Option"} | \
    {f"{s}{n}" for s in "iu" for n in (8, 16, 32, 64, 128)}


def tls_macros(files):
    """-> ({name: (form, at)}, [(path, problem)], {(path, "$x")}) of the thread-local macros `files` define (module doc):
    form "rep" when the matcher opens with a `$( .. )` repetition whose first fragment is the static's `$x` (`at`: the
    repetition's fragment kinds; an invocation entry per `;`), form "one" when the fragments before `$x` are idents and
    punctuation (`at`: the static's token index in an invocation); the last set holds each one's template static, read or
    not (an unreadable one is in the problems)."""
    out, bad, templates, tried = {}, [], set(), set()
    tls = re.compile(rb"thread_local!\s*\{\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?static\s+\$([A-Za-z_][A-Za-z0-9_]*)\s*:")
    for p, raw in sorted(files.items()):
        if b"macro_rules!" not in raw:
            continue
        for m in _MACRO.finditer(raw):
            k = _bclose(raw, m.end() - 1)
            body = raw[m.end():k] if k > 0 else b""
            st = tls.search(body)
            if st is None:
                continue
            name, x = m.group(1).decode(), st.group(1)
            templates.add((p, "$" + x.decode()))
            tried.add(name)
            _macro_form(out, bad, p, name, body, x)
    # a macro whose body hands its own `$y` to a thread-local macro's static place (a "one" form) declares through it
    # (vacuum lib.rs vacuum_guc_int! -> guc_tables session_guc_int!), to a fixpoint
    while True:
        one = {n: at for n, (form, at) in out.items() if form == "one"}
        call = re.compile(rb"(?<![A-Za-z0-9_$])(?:[A-Za-z_][A-Za-z0-9_]*::)*(" + b"|".join(re.escape(n.encode()) for n in sorted(one))
                          + rb")!\s*[({\[]") if one else None
        grew = False
        for p, raw in sorted(files.items()):
            if call is None or b"macro_rules!" not in raw:
                continue
            for m in _MACRO.finditer(raw):
                name = m.group(1).decode()
                if name in tried:
                    continue
                k = _bclose(raw, m.end() - 1)
                body = raw[m.end():k] if k > 0 else b""
                for c in call.finditer(body):
                    e = _bclose(body, c.end() - 1)
                    toks = _tokens(body[c.end():e]) if e > 0 else []
                    at = one[c.group(1).decode()]
                    if len(toks) > at and re.fullmatch(rb"\$[A-Za-z_][A-Za-z0-9_]*", toks[at]):
                        tried.add(name)
                        grew = _macro_form(out, bad, p, name, body, toks[at][1:]) or grew
                        break
        if not grew:
            return out, bad, templates


def _macro_form(out, bad, p, name, body, x):
    """thread-local macro `name` (file p, macro_rules! body `body`) declares `$x`: its form into `out` (tls_macros), or a
    problem into `bad`; -> whether `out` grew."""
    arms = [a for a in _split(body, 0x3B) if a.strip()]   # rules are `(matcher) => {..}` separated by `;`
    matcher = arms[0].split(b"=>")[0].strip() if arms else b""
    inner = matcher[1:-1].strip() if matcher[:1] in b"([{" else matcher
    frags = _FRAG.findall(inner)
    toks = _tokens(inner)
    pos = next((i for i, t in enumerate(toks) if t.startswith(b"$" + x + b":")), None)
    if len(arms) == 1 and inner.startswith(b"$(") and frags and frags[0][0] == x:
        out[name] = ("rep", [kind.decode() for _, kind in frags])
    elif len(arms) == 1 and pos is not None and all(t.endswith(b":ident") or len(t) == 1 for t in toks[:pos]):
        out[name] = ("one", pos)
    else:
        bad.append((p, f"thread-local macro {name}!: RT reads one rule whose static ($" + x.decode() + ") opens its "
                       "matcher's repetition or follows idents and punctuation only: declare its statics with "
                       "thread_local! or so"))
        return False
    return True


def macro_statics(raw, macros):
    """-> [(static, [its accessor names])] the invocations in `raw` of `macros` (`tls_macros`) declare: a "rep" entry's
    other `:ident` fragments, a "one" invocation's other identifiers (but primitive types and literals)."""
    out = []
    names = b"|".join(re.escape(n.encode()) for n in macros)
    if not names:
        return out
    for m in re.finditer(rb"(?<![A-Za-z0-9_$])(?:[A-Za-z_][A-Za-z0-9_]*::)*(" + names + rb")!\s*[({\[]", raw):
        line = raw[raw.rfind(b"\n", 0, m.start()) + 1:m.start()]
        if b"macro_rules" in line:
            continue
        k = _bclose(raw, m.end() - 1)
        if k < 0:
            continue
        form, at = macros[m.group(1).decode()]
        body = re.sub(rb"//[^\n]*|/\*.*?\*/", b"", raw[m.end():k], flags=re.S)
        if form == "one":
            toks = _tokens(body)
            if len(toks) > at and re.fullmatch(rb"[A-Za-z_][A-Za-z0-9_]*", toks[at]):
                st = toks[at].decode()
                rest = dict.fromkeys(i.decode() for i in re.findall(rb"(?<![A-Za-z0-9_.])[A-Za-z_][A-Za-z0-9_]*", body))
                out.append((st, [i for i in rest if i != st and i not in _PRIMS]))
            continue
        for entry in _split(body, 0x3B):
            idents = []
            for f, kind in zip(_split(entry, 0x2C), at):
                i = _IDENT_B.match(f.strip())
                idents.append(i.group(1).decode() if i and kind == "ident" else None)
            if idents and idents[0]:
                out.append((idents[0], [x for x in idents[1:] if x]))
    return out


def scan(root, wrapped=None, bad=None, aliases=None):
    """-> {path under crates/: [[static, ..] per block]} of the scope's files that declare a thread-local; the statics of
    a block a leader_tls! wraps go in `wrapped` ({(path, static)}). A thread-local macro's invocation entries are blocks
    of one static each (their accessors in `aliases`, {(path, static): [name, ..]}); an unreadable one goes in `bad`."""
    out = {}
    files = scope_files(root)
    macros, mbad, templates = tls_macros(files)
    if bad is not None:
        bad.extend(mbad)
    for p, raw in sorted(files.items()):
        if b"thread_local!" not in raw and not any(n.encode() + b"!" in raw for n in macros):
            continue
        starts = []
        found = tls_blocks(raw, starts)
        blocks = [[s for s in b if not s.startswith("$")] for b in found]
        for t in dict.fromkeys(s for b in found for s in b if s.startswith("$") and (p, s) not in templates):
            if bad is not None:
                bad.append((p, f"a thread-local `static {t}` in no macro_rules! RT reads (module doc)"))
        for name, acc in macro_statics(raw, macros):
            blocks.append([name])
            if aliases is not None:
                aliases[(p, name)] = acc
        blocks = [b for b in blocks if b]
        if blocks:
            out[p] = blocks
        if wrapped is not None:
            for b, at in zip(found, starts):
                if _WRAP.search(raw, max(0, at - 160), at):
                    wrapped.update((p, s) for s in b)
    return out


def file_name(path):
    """A registry / debt file's name for a source path under crates/."""
    return path.replace("/", ".")


def read_dir(root, path, skip=("README", "OWNERS")):
    """-> [(file name, line no, line)] of the directory's rows (skip, dot files, blank and '#' lines out)."""
    d = os.path.join(root, path)
    out = []
    if not os.path.isdir(d):
        return out
    for name in sorted(os.listdir(d)):
        full = os.path.join(d, name)
        if name in skip or name.startswith(".") or not os.path.isfile(full):
            continue
        with open(full, encoding="utf-8", errors="replace") as fh:
            for k, line in enumerate(fh.read().split("\n"), 1):
                if line.strip() and not line.startswith("#"):
                    out.append((name, k, line))
    return out


def read_owners(root):
    """-> ({tag}, [(line, problem)]) of OWNERS: <tag> <what it covers> <lane today>; (None, []) with no OWNERS."""
    tags, bad = set(), []
    p = os.path.join(root, OWNERS)
    if not os.path.isfile(p):
        return None, []
    with open(p, encoding="utf-8", errors="replace") as fh:
        for k, line in enumerate(fh.read().split("\n"), 1):
            if not line.strip() or line.startswith("#"):
                continue
            f = line.split("\t")
            if len(f) < 3 or not all(x.strip() for x in f[:3]) or not _IDENT.match(f[0]):
                bad.append((k, "an OWNERS line is <tag>\\t<what it covers>\\t<lane that answers today (- : none)>"))
                continue
            if f[0] in tags:
                bad.append((k, f"OWNERS: tag {f[0]} twice"))
            tags.add(f[0])
    return tags, bad


def parse_reset(reset):
    """-> (kind, arg) or None for a malformed reset."""
    kind, _, arg = reset.partition(":")
    if kind in BARE_RESETS:
        return (kind, "") if not _ else None
    if kind == "key":
        return (kind, arg) if arg.strip() else None
    if kind in FN_RESETS:
        fn, _, at = arg.partition("@")
        if _IDENT.match(fn) and (not _ or at.endswith(".rs")):
            return (kind, arg)
    return None


def agree(carry, kind):
    """-> the problem, or None: carry and reset agree (module doc)."""
    if (carry == "claim") != (kind == "claim"):
        return "carry claim goes with reset claim (the member arms it and puts it back)"
    if (carry == "test") != (kind == "test"):
        return "carry test goes with reset test"
    if (carry == "scratch") != (kind == "call"):
        return "carry scratch goes with reset call"
    if carry == "cache" and kind != "key":
        return "carry cache goes with reset key:<what every read checks>"
    if kind == "exit" and carry not in ("leader", "thread"):
        return "reset exit (it lives as long as its thread) is only for carry leader or thread"
    return None


_SRC = {}


def _src(root, path):
    """The Src of a source file under crates/ (a scope file as scope_files read it, else the working tree's), or None."""
    if path not in _SRC:
        raw = scope_files(root).get(path)
        full = os.path.join(root, "crates", path)
        if raw is None and os.path.isfile(full):
            with open(full, "rb") as fh:
                raw = fh.read()
        _SRC[path] = Src("crates/" + path, raw.decode("utf-8", "replace")) if raw is not None else None
    return _SRC[path]


def _names(code, a, b, names):
    return any(re.search(r"(?<![A-Za-z0-9_])%s(?![A-Za-z0-9_])" % re.escape(s), code[a:b]) for s in names)


def reset_holds(root, path, block, arg):
    """-> the problem, or None: the reset `arg` of a static of `path` is a function (any case: PostgreSQL's are CamelCase)
    or a type with an `impl Drop`, in the static's file (or <fn>@<path>), whose body names the static or a static of its
    block (`block`), directly or through one function of the static's file that does (an accessor such as `with_cache`)."""
    fn, _, at = arg.partition("@")
    where = at or path
    src = _src(root, where)
    if src is None:
        return f"no source file crates/{where} for its reset {arg}"
    code = src.joined("code")
    fns = functions(src)
    spans = [(a, b) for n, _, (a, b) in fns if n == fn]
    for m in re.finditer(r"\bimpl\b[^{;]*?\bDrop\s+for\s+%s\b[^{;]*\{" % re.escape(fn), code):
        k = _close(code, m.end() - 1, "{", "}")
        if k > 0:
            spans.append((m.end() - 1, k))
    if not spans:
        return f"no function `{fn}` (nor `impl Drop for {fn}`) in crates/{where} (its reset)"
    if any(_names(code, a, b, block) for a, b in spans):
        return None
    own = _src(root, path)              # the accessors are the static's own file's functions (a fn@path reset calls one)
    ocode = own.joined("code") if own is not None else ""
    accessors = {n for n, _, (a, b) in (functions(own) if own is not None else []) if not (n == fn and own is src)
                 and _names(ocode, a, b, block)}
    if any(re.search(r"(?<![A-Za-z0-9_])%s\s*(?:::<[^>]*>\s*)?\(" % re.escape(n), code[a:b]) for a, b in spans for n in accessors):
        return None
    return f"`{fn}` in crates/{where} names none of {' / '.join(block)}, nor calls a function of the file that does: it does not reset it"


SHAPES = {"registry": "<path>\\t<static>\\t<owner>\\t<reset>\\t<carry>\\t[<member | note>]",
          "TLS-DEBT": "<path>\\t<static>\\t<owner lane>\\t<carry today>\\t<what is missing>",
          "LEADER-TLS-DEBT": "<path>\\t<static>\\t<owner>\\t<note>",
          "SCOPE-DEBT": "<path>\\t<static>\\t<owner lane>\\t<what is missing>",
          "GUARD-DEBT": "<path>\\t<static>\\t<owner lane>\\t<what is missing>"}


def parse_rows(rows, width, what):
    """-> ({(path, static): (file, line, fields)}, [(file, line, problem)])."""
    out, bad = {}, []
    for name, k, line in rows:
        f = line.split("\t")
        if len(f) < width or not all(x.strip() for x in f[:width]):
            bad.append((name, k, f"a {what} row is " + SHAPES[what]))
            continue
        key = (f[0], f[1])
        if key in out:
            bad.append((name, k, f"two {what} rows for {f[0]} {f[1]}"))
            continue
        if name != file_name(f[0]):
            bad.append((name, k, f"the {what} row of {f[0]} {f[1]} goes in the file {file_name(f[0])}"))
        out[key] = (name, k, f)
    return out, bad


def base_rows(ctx, path, width):
    """The parent's rows of a registry or debt directory ({(path, static): fields}), or None when it has none."""
    text = ctx.base_ledger(path) if ctx.base_ledger else None
    if text is None:
        return None
    out = {}
    for line in text.split("\n"):
        f = line.split("\t")
        if line.strip() and not line.startswith("#") and len(f) >= width:
            out[(f[0], f[1])] = f
    return out


def guarded(root, path, block, arg):
    """The extent's end runs on any exit: `arg` (<name>[@<path>], a scope reset) is a type with an `impl Drop` in its file
    (reset_holds checks the drop puts the static back), or a function whose body names such a type of its file whose
    drop puts the static (or one of its `block`) back."""
    fn, _, at = arg.partition("@")
    src = _src(root, at or path)
    if src is None:
        return False
    code = src.joined("code")
    types = {m.group(1) for m in re.finditer(r"\bimpl\b[^{;]*?\bDrop\s+for\s+([A-Za-z_][A-Za-z0-9_]*)\b", code)}
    if fn in types:
        return True
    spans = [(a, b) for n, _, (a, b) in functions(src) if n == fn]
    return any(_names(code, a, b, [t]) and reset_holds(root, path, block, t + ("@" + at if at else "")) is None
               for t in sorted(types) for a, b in spans)


def rig_calls(root, names):
    """-> [(path, name)] of each production call (cfg(test) items and tests/ out) of a test rig's bare-pair function."""
    out = []
    for p, raw in sorted(scope_files(root).items()):
        hit = [n for n in names if n.encode() + b"(" in raw]
        if not hit:
            continue
        code = _src(root, p).joined("code")
        for n in hit:
            if re.search(r"(?<![A-Za-z0-9_])(?<!fn )%s\s*\(" % re.escape(n), code):
                out.append((p, n))
    return out


def static_type(root, path, static):
    """-> the declared type of `static <static>:` in crates/<path> (its code, comments out), or None (a macro's)."""
    src = _src(root, path)
    if src is None:
        return None
    m = re.search(r"\bstatic\s+%s\s*:\s*(.*?)\s*=" % re.escape(static), src.joined("code"), re.S)
    return " ".join(m.group(1).split()) if m else None


def check_tree(ctx):
    root, out = ctx.root, []
    _SRC.clear()
    _FILES.clear()
    wrapped, mbad, aliases = set(), [], {}
    found = scan(root, wrapped, mbad, aliases)
    out += [(f"crates/{p}", 0, note) for p, note in mbad]
    blocks = {(p, s): b for p, bs in found.items() for b in bs for s in b}
    tags, obad = read_owners(root)
    out += [(OWNERS, k, note) for k, note in obad]
    reg, bad = parse_rows(read_dir(root, REGISTRY), 5, "registry")
    debt, dbad = parse_rows(read_dir(root, DEBT), 5, "TLS-DEBT")
    out += [(f"{REGISTRY}/{n}", k, note) for n, k, note in bad]
    out += [(f"{DEBT}/{n}", k, note) for n, k, note in dbad]
    ldebt, lbad = parse_rows(read_dir(root, LEADER_DEBT), 4, "LEADER-TLS-DEBT")
    out += [(f"{LEADER_DEBT}/{n}", k, note) for n, k, note in lbad]
    sdebt, sbad = parse_rows(read_dir(root, SCOPE_DEBT), 4, "SCOPE-DEBT")
    out += [(f"{SCOPE_DEBT}/{n}", k, note) for n, k, note in sbad]
    gdebt, gbad = parse_rows(read_dir(root, GUARD_DEBT), 4, "GUARD-DEBT")
    out += [(f"{GUARD_DEBT}/{n}", k, note) for n, k, note in gbad]
    rigs = {}
    for (p, s), (name, k, f) in sorted(reg.items()):
        loc = f"{REGISTRY}/{name}"
        owner, reset, carry = f[2], f[3], f[4]
        member = f[5].strip() if len(f) > 5 else ""
        if (p, s) not in blocks:
            out.append((loc, k, f"{p} {s}: no such thread-local in the scope (gone, moved or out of scope): delete the row"))
            continue
        for d, at, what in ((debt, DEBT, "TLS-DEBT"), (sdebt, SCOPE_DEBT, "SCOPE-DEBT")):
            if (p, s) in d:
                out.append((loc, k, f"{p} {s}: registered, so its {what} row goes (delete {at}/{file_name(p)}'s line)"))
        if tags is None or owner not in tags:
            out.append((loc, k, f"{p} {s}: owner {owner} is not a tag of {OWNERS}" + (" (no such file)" if tags is None else "")))
        if carry not in CARRY:
            out.append((loc, k, f"{p} {s}: carry {carry} is not one of {' | '.join(CARRY)}"))
            continue
        if (p, s) in wrapped and carry not in ("leader", "test"):
            out.append((loc, k, f"{p} {s}: leader_tls! declares leader-class statics only (carry {carry}): declare it "
                                "with a plain thread_local!"))
        elif carry == "leader" and p.startswith(LEADER_SCOPE):
            if (p, s) in wrapped and (p, s) in ldebt:
                out.append((loc, k, f"{p} {s}: declared through leader_tls!, so its LEADER-TLS-DEBT row goes (delete "
                                    f"{LEADER_DEBT}/{file_name(p)}'s line)"))
            elif (p, s) not in wrapped and (p, s) not in ldebt:
                out.append((loc, k, f"{p} {s}: a leader-class static of crates/gravity is declared through leader_tls! "
                                    "(leader_tls::leader_tls! { thread_local! { .. } }: a debug build raises on a touch "
                                    "inside a pool claim)"))
        if carry in ("claim", "debt") and not member:
            out.append((loc, k, f"{p} {s}: a carry {carry} row names " + ("its CLAIM_REGISTRY member" if carry == "claim"
                                                                         else "what is missing") + " in its last field"))
        r = parse_reset(reset)
        if r is None:
            out.append((loc, k, f"{p} {s}: reset {reset!r} is not scope:<fn> | statement:<fn> | xact:<fn> | at:<fn> | "
                                "claim | key:<what> | call | exit | test"))
            continue
        problem = agree(carry, r[0]) if carry != "debt" else None
        if problem:
            out.append((loc, k, f"{p} {s}: {problem}"))
        if r[0] == "scope" and (p, s) not in gdebt and not guarded(root, p, blocks[(p, s)] + aliases.get((p, s), []), r[1]):
            out.append((loc, k, f"{p} {s}: reset {reset} is no Drop guard: an ERROR that unwinds out of the extent skips "
                                "its end (C7 (d), D4113): name a type whose impl Drop puts it back, scope:<Guard>"))
        rig = _RIG.search(member)
        if rig:
            rigs.update({n.strip(): (loc, k, p, s) for n in rig.group(1).split(",")})
        if r[0] in ("exit", "at"):
            if _PER_RUN.search(member):
                out.append((loc, k, f"{p} {s}: per-run / per-statement data with reset {reset} outlives its statement "
                                    "(C7 (e), D4114): move it into the run's state, or reset at statement: / scope:"))
            t = static_type(root, p, s)
            if t is not None and _GROWS.search(t) and not _BOUND.search(member):
                out.append((loc, k, f"{p} {s}: a growing {t} kept across statements (reset {reset}) with no stated bound "
                                    "(C7 (e), D4114): its note states a cap, 'per session by design' or 'test mode', or "
                                    "the data moves into the run"))
        if r[0] in FN_RESETS:
            problem = reset_holds(root, p, blocks[(p, s)] + aliases.get((p, s), []), r[1])
            if problem:
                out.append((loc, k, f"{p} {s}: reset {reset}: {problem}"))
    for (p, s), (name, k, f) in sorted(debt.items()):
        if (p, s) not in blocks:
            out.append((f"{DEBT}/{name}", k, f"{p} {s}: no such thread-local in the scope: delete the row"))
        if (p, s) in sdebt:
            out.append((f"{DEBT}/{name}", k, f"{p} {s}: a row in TLS-DEBT and in SCOPE-DEBT: one goes"))
    for (p, s), (name, k, f) in sorted(gdebt.items()):
        r = parse_reset(reg[(p, s)][2][3]) if (p, s) in reg and (p, s) in blocks else None
        if r is not None and r[0] == "scope" and guarded(root, p, blocks[(p, s)] + aliases.get((p, s), []), r[1]):
            out.append((f"{GUARD_DEBT}/{name}", k, f"{p} {s}: its reset is a Drop guard now: delete the GUARD-DEBT row"))
        elif (p, s) not in reg and (p, s) in blocks:
            out.append((f"{GUARD_DEBT}/{name}", k, f"{p} {s}: no registry row: a GUARD-DEBT row names a registered static"))
    gspent = sum(1 for key in gdebt if key not in reg and key not in blocks)
    for n, (loc, k, p, s) in sorted(rigs.items()):
        fns = {f for f, _, _ in functions(_src(root, p))} if _src(root, p) is not None else set()
        if n not in fns:
            out.append((loc, k, f"{p} {s}: rig: {n}: no such function in crates/{p}"))
    for q, n in rig_calls(root, sorted(rigs)):
        loc, k, p, s = rigs[n]
        out.append((f"crates/{q}", 0, f"{n}: the bare pair of {p} {s} is the in-process test rigs' (its row's rig:); "
                                      "production code holds the guard (C7 (d))"))
    # a SCOPE-DEBT row whose static went (another lane deleted or renamed it) is spent, as a LEADER-TLS-DEBT row below
    sspent = sum(1 for key in sdebt if key not in blocks)
    spent = 0
    for (p, s), (name, k, f) in sorted(ldebt.items()):
        if (p, s) not in blocks and (p, s) not in reg:
            # The static and its registry row went (another lane deleted it): the row is spent, not wrong, so the lane
            # deleting a static need not edit this list; the note counts it and the next edit of the list drops it.
            spent += 1
            continue
        if not p.startswith(LEADER_SCOPE) or (p, s) not in reg or reg[(p, s)][2][4] != "leader":
            out.append((f"{LEADER_DEBT}/{name}", k, f"{p} {s}: no leader-class registry row of "
                        f"{' '.join('crates/' + x for x in LEADER_SCOPE)} (gone, moved or another carry): delete the row"))
    for p, bs in sorted(found.items()):
        for b in bs:
            for s in dict.fromkeys(b):
                if (p, s) not in reg and (p, s) not in debt and (p, s) not in sdebt:
                    out.append((f"crates/{p}", 0, f"thread-local {s} is in no registry row: add "
                                f"'{p}\\t{s}\\t<owner>\\t<reset>\\t<carry>\\t[<member | note>]' to {REGISTRY}/{file_name(p)}"))
    carry_debt = sorted(key for key, (_, _, f) in reg.items() if f[4] == "debt")
    sbase = base_rows(ctx, SCOPE_DEBT, 4)
    sbase_n = sum(1 for key in sbase if key in blocks) if sbase is not None else None
    ctx.notes.append(f"RT: TLS-DEBT {len(debt)} rows, CARRY-DEBT {len(carry_debt)} rows, "
                     f"LEADER-TLS-DEBT {len(ldebt) - spent} rows" + (f" ({spent} spent: static gone)" if spent else "")
                     + f", SCOPE-DEBT {len(sdebt) - sspent} rows" + (f" (parent {sbase_n})" if sbase_n is not None else "")
                     + (f" ({sspent} spent: static gone)" if sspent else "") + f", GUARD-DEBT {len(gdebt) - gspent} rows"
                     + f", registry {len(reg)} rows")
    gbase = base_rows(ctx, GUARD_DEBT, 4)
    if gbase is not None:
        for (p, s), (name, k, f) in sorted(gdebt.items()):
            if (p, s) not in gbase:
                out.append((f"{GUARD_DEBT}/{name}", k, f"{p} {s} added: GUARD-DEBT only shrinks (put it back through a "
                                                       "Drop guard)"))
    if sbase is not None:
        for (p, s), (name, k, f) in sorted(sdebt.items()):
            if (p, s) not in sbase:
                out.append((f"{SCOPE_DEBT}/{name}", k, f"{p} {s} added: SCOPE-DEBT only shrinks (register it in {REGISTRY})"))
    base = base_rows(ctx, DEBT, 5)
    if base is not None:
        for (p, s), (name, k, f) in sorted(debt.items()):
            if (p, s) not in base:
                out.append((f"{DEBT}/{name}", k, f"{p} {s} added: TLS-DEBT only shrinks (register it in {REGISTRY})"))
    base = base_rows(ctx, LEADER_DEBT, 4)
    if base is not None:
        for (p, s), (name, k, f) in sorted(ldebt.items()):
            if (p, s) not in base:
                out.append((f"{LEADER_DEBT}/{name}", k, f"{p} {s} added: LEADER-TLS-DEBT only shrinks (declare it "
                                                        "through leader_tls!)"))
    base = base_rows(ctx, REGISTRY, 5)
    if base is not None:
        for key in carry_debt:
            if key not in base or base[key][4] != "debt":
                name, k, _ = reg[key]
                out.append((f"{REGISTRY}/{name}", k, f"{key[0]} {key[1]}: carry debt added: CARRY-DEBT only shrinks"))
    return out


# Selftest cases (run.py --selftest): a case is (path, text) or {"files": [(path, text), ..], "base": {dir: rows text},
# "want": a text the failure must hold}. The planted ones are the checks the claim census made before RT (hosts
# claim_census_is_pinned and tools tls_census.py check_claim; lint/THREAD-STATE/README maps each to its clause here),
# then RT's own.
_S = "gravity/x/src/lib.rs"
_SF = REGISTRY + "/" + file_name(_S)
_TLS = ("thread_local! {\n    static A: Cell<u8> = const { Cell::new(0) };\n    static B: Cell<u8> = const { Cell::new(0) };\n}\n"
        "pub fn with_a<T>(v: u8, f: impl FnOnce() -> T) -> T {\n    struct Back(u8);\n    impl Drop for Back {\n"
        "        fn drop(&mut self) {\n            A.with(|a| a.set(self.0));\n        }\n    }\n"
        "    let _back = Back(A.with(|a| a.replace(v)));\n    f()\n}\n")
_OWN = (OWNERS, "x\tthe test crate\t-\n")
_ROWS = f"{_S}\tA\tx\tscope:with_a\tleader\tthe admission running now\n{_S}\tB\tx\tscope:with_a\tleader\tits sibling\n"


_LF = LEADER_DEBT + "/" + file_name(_S)
_TLSW = "leader_tls::leader_tls! {\n    " + _TLS.split("pub fn")[0].replace("\n", "\n    ").rstrip() + "\n}\n" + \
    "pub fn" + _TLS.split("pub fn", 1)[1]


def _case(rows, want=None, src=_TLS, base=None, more=(), leader=True):
    """A case of the registry file `rows` over `src`; leader: each leader row of an unwrapped src gets its
    LEADER-TLS-DEBT row (the cases before the runtime half), so only the leader cases below meet that clause."""
    ldebt = "".join(f"{f[0]}\t{f[1]}\tisojoin\tnot yet\n" for f in (l.split("\t") for l in rows.split("\n"))
                    if len(f) > 4 and f[4] == "leader") if leader and "leader_tls!" not in src else ""
    c = {"files": [("crates/" + _S, src), _OWN, (_SF, rows)] + ([(_LF, ldebt)] if ldebt else []) + list(more)}
    if want:
        c["want"] = want
    if base is not None:
        c["base"] = base
    return c


# C7 coverage E1: a thread-local macro of scalar_global!'s shape, its entries' statics in the invoking file
_MAC = ("macro_rules! global {\n    ($($cell:ident, $get:ident, $set:ident, $ty:ty, $init:expr;)+) => {\n        $(\n"
        "            thread_local! {\n                static $cell: Cell<$ty> = const { Cell::new($init) };\n            }\n"
        "            pub fn $get() -> $ty {\n                $cell.get()\n            }\n"
        "            pub fn $set(v: $ty) {\n                $cell.set(v)\n            }\n        )+\n    };\n}\n"
        "global! {\n    // a comment between entries\n    G1, g1, set_g1, u8, 0;\n    G2, g2, set_g2, u8, 0;\n}\n"
        "pub fn reset_g2() {\n    set_g2(0);\n}\n")
_MROWS = f"{_S}\tG1\tx\texit\tthread\tits\n{_S}\tG2\tx\tat:reset_g2\tthread\tits accessor resets it\n"
_MUSE = ("crates/gravity/x/src/other.rs", "crate::global! {\n    G3, g3, set_g3, u8, 0;\n}\n")
# a macro whose body hands its `$cell` to a thread-local macro (vacuum_guc_int! -> session_guc_int!)
_MONE = ("macro_rules! one_guc {\n    ($cell:ident, $get:ident, $boot:expr) => {\n        thread_local! {\n"
         "            static $cell: Cell<i32> = const { Cell::new($boot) };\n        }\n"
         "        pub fn $get() -> i32 {\n            $cell.get()\n        }\n    };\n}\n")
_MNEST = ("crates/gravity/x/src/nest.rs", "macro_rules! nest_guc {\n    ($($cell:ident, $get:ident, $boot:expr;)+) => {\n"
          "        $( crate::one_guc!($cell, $get, $boot); )+\n    };\n}\nnest_guc! {\n    N1, n1, 0;\n}\n")
_MBAD = ("macro_rules! bad {\n    ($get:expr, $cell:ident) => {\n        thread_local! {\n"
         "            static $cell: Cell<u8> = const { Cell::new($get) };\n        }\n    };\n}\nbad!(1 + 1, B9);\n")

# C7 coverage E2: a crate outside the scope's old prefixes, its statics in SCOPE-DEBT
_SP = "backend/parser/src/y.rs"
_SPD = SCOPE_DEBT + "/" + file_name(_SP)
_SDROWS = f"{_SP}\tA\tisojoin\towner, reset, carry\n{_SP}\tB\tisojoin\towner, reset, carry\n"


def _sd_case(rows, want=None, base=None, more=()):
    c = {"files": [("crates/" + _SP, _TLS), _OWN, (_SPD, rows)] + list(more)}
    if want:
        c["want"] = want
    if base is not None:
        c["base"] = base
    return c


# C7 clause (d): a bare begin / end pair over B, and a test rig's bare pair
_BARE = _TLS + "pub fn bare_b<T>(f: impl FnOnce() -> T) -> T {\n    B.with(|b| b.set(1));\n    let r = f();\n" \
    "    B.with(|b| b.set(0));\n    r\n}\n"
_BROWS = _ROWS.replace("scope:with_a\tleader\tits sibling", "scope:bare_b\tleader\tits sibling")
_GD = (GUARD_DEBT + "/" + file_name(_S), f"{_S}\tB\tisojoin\ta Drop guard\n")
_RIGS = _TLS + "pub fn b_begin() {\n    B.with(|b| b.set(1));\n}\n"
_RROWS = _ROWS.replace("\tleader\tits sibling", "\tleader\tits sibling; rig: b_begin")
_RUSE = "pub fn run() {\n    crate::b_begin();\n}\n"

# C7 clause (e): a growing static kept across statements
_GROW = _TLS + "thread_local! {\n    static C: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };\n}\n"
_GROWS_ROWS = _ROWS + f"{_S}\tC\tx\texit\tthread\tthe names seen\n"


PLANTED_TOML = _case(_ROWS.split("\n")[0] + "\n", "thread-local B is in no registry row")       # check_claim / test: unclassified
CLEAN_TOML = _case(_ROWS)
MORE_PLANTED_TOML = [
    _case(_ROWS + f"{_S}\tGONE\tx\texit\tleader\n", "GONE: no such thread-local"),                    # stale line
    _case(_ROWS + f"{_S}\tA\tx\texit\tleader\n", "two registry rows for"),                           # two lines / classes
    _case(_ROWS.replace("\tleader\tits", "\tsession\tits"), "carry session is not one of"),          # a class not in the list
    _case(_ROWS.replace("scope:with_a\tleader\tits sibling", "claim\tclaim\t"), "names its CLAIM_REGISTRY member"),
    _case(_ROWS.replace("\tleader\tits sibling", "\tdebt\t"), "names what is missing"),                 # debt without a note
    _case(_ROWS, "no such thread-local in the scope", more=[                                          # out of scope
        ("crates/backend/parser/tests/y.rs", _TLS),
        (REGISTRY + "/backend.parser.tests.y.rs", "backend/parser/tests/y.rs\tA\tx\tscope:with_a\tleader\tout of scope\n")]),
    {"files": [("crates/backend/utils/adt/src/y.rs", _TLS), _OWN], "want": "thread-local A is in no registry row"},  # #scope
    _case(_ROWS.replace("\tleader\tits sibling", "\tdebt\tthe database's"), "carry debt added",       # #carry-debt pin
          base={REGISTRY: _ROWS, DEBT: ""}),
    _case(_ROWS.replace("\tx\tscope:with_a\tleader\tits", "\tnobody\tscope:with_a\tleader\tits"), "is not a tag of"),
    _case(_ROWS.replace("scope:with_a\tleader\tits", "scope:gone\tleader\tits"), "no function `gone`"),
    _case(_ROWS.replace("scope:with_a\tleader\tits", "statement:with_b\tleader\tits"), "names none of",
          src=_TLS + "pub fn with_b() -> u8 {\n    // B.with(..) in a comment is no reset\n    0\n}\n"),
    _case(_ROWS.replace("scope:with_a\tleader\tits", "soon\tleader\tits"), "is not scope:<fn>"),
    _case(_ROWS.replace("scope:with_a\tleader\tits", "call\tleader\tits"), "carry scratch goes with reset call"),
    _case(_ROWS.replace("scope:with_a\tleader\tits", "exit\tcache\tits"), "carry cache goes with reset key:"),
    _case(_ROWS.replace("scope:with_a\tleader\tits", "exit\tclaim\tits"), "carry claim goes with reset claim"),
    _case(_ROWS.split("\n")[0] + "\n", "added: TLS-DEBT only shrinks",                                 # a new debt row
          more=[(DEBT + "/" + file_name(_S), f"{_S}\tB\tisojoin\tleader\towner and reset\n")], base={DEBT: ""}),
    _case(_ROWS, "registered, so its TLS-DEBT row goes",
          more=[(DEBT + "/" + file_name(_S), f"{_S}\tB\tisojoin\tleader\towner and reset\n")]),
    {"files": [("crates/" + _S, _TLS), _OWN, (REGISTRY + "/gravity.x.src.other.rs", _ROWS)], "want": "goes in the file"},
    # the runtime half: leader_tls!
    _case(_ROWS, "is declared through leader_tls!", leader=False),                                   # neither
    _case(_ROWS.replace("scope:with_a\tleader\tits", "exit\tthread\tits"), "declares leader-class statics only",
          src=_TLSW),
    _case(_ROWS, "so its LEADER-TLS-DEBT row goes", src=_TLSW, more=[(_LF, f"{_S}\tA\tisojoin\tnot yet\n")]),
    _case(_ROWS.replace("scope:with_a\tleader\tits", "exit\tthread\tits"), "no leader-class registry row",
          more=[(_LF, f"{_S}\tA\tisojoin\tnot yet\n{_S}\tB\tisojoin\tnot yet\n")], leader=False),
    _case(_ROWS, "LEADER-TLS-DEBT only shrinks", base={LEADER_DEBT: f"{_S}\tA\tisojoin\tnot yet\n"}),
    _case(_ROWS, "a LEADER-TLS-DEBT row is", more=[(_LF, f"{_S}\tA\tisojoin\n{_S}\tB\tisojoin\tnot yet\n")],
          leader=False),
    # C7 coverage E1: macro-declared thread-locals
    _case(_MROWS.split("\n")[0] + "\n", "thread-local G2 is in no registry row", src=_MAC),        # an entry's static
    _case(_MROWS + f"{_S}\t$cell\tx\texit\tthread\tthe template\n", "$cell: no such thread-local", src=_MAC),
    _case(_MROWS, "thread-local G3 is in no registry row", src=_MAC, more=[_MUSE]),               # invoked elsewhere
    _case(_MROWS, "thread-local N1 is in no registry row", src=_MAC + _MONE, more=[_MNEST]),      # through a macro
    _case(_MROWS.replace(f"{_S}\tG1\tx\texit", f"{_S}\tG1\tx\tat:reset_g2"), "names none of", src=_MAC),  # G2's accessor
    _case(_MROWS, "RT reads one rule whose static", src=_MAC + _MBAD),                   # a shape RT cannot read
    # C7 coverage E2: the whole workspace, SCOPE-DEBT
    {"files": [("crates/" + _SP, _TLS), _OWN, (_SPD, _SDROWS.split("\n")[0] + "\n")],                 # a crate E2 gained
     "want": "thread-local B is in no registry row"},
    _sd_case(_SDROWS, "added: SCOPE-DEBT only shrinks", base={SCOPE_DEBT: _SDROWS.split("\n")[0] + "\n"}),
    _sd_case(_SDROWS, "registered, so its SCOPE-DEBT row goes",
             more=[(REGISTRY + "/" + file_name(_SP), _ROWS.replace(_S, _SP))]),
    _sd_case(_SDROWS.replace("\towner, reset, carry", ""), "a SCOPE-DEBT row is"),
    _sd_case(_SDROWS, "a row in TLS-DEBT and in SCOPE-DEBT",
             more=[(DEBT + "/" + file_name(_SP), f"{_SP}\tA\tisojoin\tleader\towner and reset\n")]),
    # C7 clause (d), unwind-safe thread scopes (D4113 E4)
    _case(_BROWS, "is no Drop guard", src=_BARE),                                                     # a bare pair
    _case(_BROWS, "GUARD-DEBT only shrinks", src=_BARE, more=[_GD], base={GUARD_DEBT: ""}),          # a new debt row
    _case(_ROWS, "its reset is a Drop guard now", more=[_GD]),                                       # guarded: row goes
    _case(_RROWS, "the in-process test rigs'", src=_RIGS, more=[("crates/gravity/x/src/run.rs", _RUSE)]),  # rig call
    _case(_RROWS.replace("rig: b_begin", "rig: b_gone"), "rig: b_gone: no such function", src=_RIGS),
    # C7 clause (e), state kept across statements (D4114)
    _case(_GROWS_ROWS, "with no stated bound", src=_GROW),                                            # no bound
    _case(_GROWS_ROWS.replace("the names seen", "per-run data; capped at 8 names"), "per-run / per-statement data",
          src=_GROW),                                                                                 # run data kept
]
MORE_CLEAN_TOML = [
    _case(_ROWS.split("\n")[0] + "\n", more=[(DEBT + "/" + file_name(_S), f"{_S}\tB\tisojoin\tleader\towner and reset\n")],
          base={DEBT: f"{_S}\tB\tisojoin\tleader\towner and reset\n"}),                             # an old debt row stays
    _case(_ROWS.replace("scope:with_a\tleader\tits", "at:Restore\tthread\tits"),                     # a Drop type
          src=_TLS + "struct Restore(u8);\nimpl Drop for Restore {\n    fn drop(&mut self) {\n"
                     "        B.with(|b| b.set(self.0));\n    }\n}\n"),
    _case(_ROWS.replace("scope:with_a\tleader\tits", "key:the catalog epoch\tcache\tits")),
    _case(_ROWS.replace("scope:with_a\tleader\tits", "at:AtEOXact_B\tleader\tits"),                 # PostgreSQL's CamelCase
          src=_TLS + "pub fn AtEOXact_B() {\n    B.with(|b| b.set(0));\n}\n"),
    _case(_ROWS.replace("scope:with_a\tleader\tits", "at:Invalidate@gravity/x/src/inval.rs\tleader\tits"),
          src=_TLS + "pub fn with_b<T>(f: impl FnOnce(&Cell<u8>) -> T) -> T {\n    B.with(f)\n}\n",
          more=[("crates/gravity/x/src/inval.rs", "pub fn Invalidate() {\n    crate::with_b(|b| b.set(0));\n}\n")]),
    _case(_ROWS.replace("scope:with_a\tleader\tits", "at:forget\tleader\tits"),                     # through an accessor
          src=_TLS + "fn with_b<T>(f: impl FnOnce(&Cell<u8>) -> T) -> T {\n    B.with(f)\n}\n"
                     "pub fn forget() {\n    with_b(|b| b.set(0));\n}\n"),
    _case(_ROWS, more=[("crates/gravity/x/tests/t.rs", _TLS)]),                                       # tests/ need no row
    _case(_ROWS, src=_TLSW),                                                                          # leader_tls!
    _case(_ROWS, base={LEADER_DEBT: f"{_S}\tA\tisojoin\tnot yet\n{_S}\tB\tisojoin\tnot yet\n"}),  # old rows stay
    {"files": [("crates/backend/utils/adt/src/y.rs", _TLS), _OWN,                                    # backend: step 2
               (REGISTRY + "/backend.utils.adt.src.y.rs", _ROWS.replace(_S, "backend/utils/adt/src/y.rs"))]},
    _case(_ROWS, more=[(_LF, f"{_S}\tA\tisojoin\tnot yet\n{_S}\tB\tisojoin\tnot yet\n{_S}\tGONE\tisojoin\tnot yet\n")],
          leader=False),                                                                              # a spent row
    _case(_MROWS, src=_MAC),                                     # E1: each entry its row, a reset through its accessor
    _case(_MROWS, src=_MAC, more=[_MUSE, (REGISTRY + "/gravity.x.src.other.rs", "gravity/x/src/other.rs\tG3\tx\texit\tthread\tits\n")]),
    _case(_MROWS, src=_MAC + _MONE, more=[_MNEST, (REGISTRY + "/gravity.x.src.nest.rs", "gravity/x/src/nest.rs\tN1\tx\texit\tthread\tits\n")]),
    _sd_case(_SDROWS, base={SCOPE_DEBT: _SDROWS}),                                      # E2: old SCOPE-DEBT rows stay
    _sd_case(_SDROWS + f"{_SP}\tGONE\tisojoin\towner, reset, carry\n",                         # a spent row
             base={SCOPE_DEBT: _SDROWS + f"{_SP}\tGONE\tisojoin\towner, reset, carry\n"}),
    _case(_BROWS, src=_BARE, more=[_GD], base={GUARD_DEBT: _GD[1]}),                                  # (d): an old debt row
    _case(_RROWS, src=_RIGS, more=[("crates/gravity/x/src/run.rs", "#[cfg(test)]\nmod t {\n" + _RUSE + "}\n")]),
    _case(_ROWS.replace("scope:with_a", "scope:Back"),                                               # a guard type
          src=_TLS + "pub struct Back(u8);\nimpl Drop for Back {\n    fn drop(&mut self) {\n"
                     "        B.with(|b| b.set(self.0));\n    }\n}\n"),
    _case(_GROWS_ROWS.replace("the names seen", "the names seen; capped at 8 names"), src=_GROW),       # (e): a cap
    _case(_GROWS_ROWS.replace("the names seen", "per session by design (the C hash)"), src=_GROW),     # (e): by design
    _case(_ROWS.replace("scope:with_a\tleader\tits", "exit\tthread\tits")),                       # (e): a Cell, no growth
]
