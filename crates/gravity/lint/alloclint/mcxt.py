#!/usr/bin/env python3
"""alloclint's mcx-transmute kind (USER APPROVED via main 2026-10-15T00:10:23Z; memlead's 2026-10-14T23:35:10Z note (b)).

A lifetime-extended memory-context handle is sound only while its context never moves after the handle is taken: an Mcx is
the context's address (`Mcx<'mcx>(&'mcx MemoryContext)`, the context inline state). The box-24 SIGABRT (pglguc C5b WIP
c2179497b3d) was `transmute::<Mcx<'_>, Mcx<'static>>(cxt.mcx())` over a context held BY VALUE, moved into an Rc after the
handle was taken. Rule (a): a lifetime-extended handle comes only from an address-stable owner (PinnedContext::handle,
McxOwned / SendRoot, a static or leaked context), which needs no transmute. This kind is the ratchet for the idiom.

The kind (memlead's K1 spec, 2026-10-15T00:26:21Z; item `transmute`): one site per transmute / transmute_copy CALL (the parsed
expression, multi-line calls included) outside crates/_support/mcx where (a) its turbofish or argument names Mcx (not
McxOwned), (b) its argument calls a method named mcx or ending in _mcx (`x.mcx()`, `(*p).mcx()`, `per_tuple_mcx()`) or is exactly an
identifier named mcx / *_mcx (`State { mcx: unsafe { transmute(mcx) } }`, memlead note (2)), or (c) it
initializes a `let` whose annotated type names Mcx, or it is the tail of a fn / closure whose return type names Mcx. A Node /
TupleDesc / fn-pointer lifetime transmute is no site. Comments and strings are blanked first (homes.blank). memlead's K2 (raw-
pointer reborrows of a MemoryContext: `&*(p as *const MemoryContext)`, `NonNull::as_ref().mcx()`) is NOT in the approved
scope (the user's word, via main). A site inside a homes.tsv function is tracked (memlead: no product site is a home; a row
leaves only by migrating to PinnedContext / McxOwned / SendRoot / a static owner). The ledger is
crates/gravity/lint/ALLOCLINT-DEBT/mcx-transmute/<file>.tsv (`path<TAB>file`, rows `mcx-transmute<TAB>transmute<TAB>n`: a count
per file, so a line move is no churn; memlead's class per site, BYVALUE / BOX / PARAM / TEST, is in
lanes/memlead/mcx-transmute-census.tsv). `ratchet.py base` judges it on every range (S0 / S1 / S2): a changed file's sites
must EQUAL its rows (NEW: a new site; STALE: lower the row). The seed (the census) was user-approved as new debt rows, one
range only.

    mcxt.py scan [--repo R] [--rev REV] [FILE ..]   the sites (file, line, item) of those files (default: every candidate)
    mcxt.py write [--repo R]                         rewrite ALLOCLINT-DEBT/mcx-transmute/ from a full scan of the worktree
    mcxt.py check [--repo R]                         the full scan against the worktree's rows (rc 0 equal, 1 differ)
    mcxt.py selftest
"""
import collections
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import homes as homes_mod  # noqa: E402

KIND = "mcx-transmute"
SUBDIR = "mcx-transmute"
LEDGER = "crates/gravity/lint/ALLOCLINT-DEBT"
HOMES = "crates/gravity/lint/alloclint/homes.tsv"
EXCLUDE = ("crates/_support/mcx/",)   # the mcx crate: the primitives (PinnedContext, McxOwned, SendRoot) themselves
RE_TM = re.compile(r"\btransmute(?:_copy)?\b")
RE_MCX_TYPE = re.compile(r"\bMcx\b")                                    # Mcx / mcx::Mcx, any lifetimes (not McxOwned)
RE_MCX_CALL = re.compile(r"\.\s*(?:[A-Za-z_]\w*_)?mcx\s*\(")             # a method named mcx or ending in _mcx
RE_MCX_IDENT = re.compile(r"\(\s*(?:[A-Za-z_]\w*_)?mcx\s*\)$")       # (b') the argument is exactly an identifier mcx / *_mcx
RE_LET = re.compile(r"\blet\s+(?:mut\s+)?[A-Za-z_]\w*\s*:([^=;]*)=[^=;]*$")  # `let x: T = ...` up to the transmute
CANDIDATE = r"transmute"   # git grep -E: the files worth reading
TAIL_BACK = 16             # lines above a transmute searched for the enclosing fn / closure's `-> .. Mcx .. {` (memlead note
                           # (1): a SAFETY comment of 4+ lines between the signature and the body; blanking keeps its lines)


def in_scope(f):
    return f.startswith("crates/") and f.endswith(".rs") and not f.startswith(EXCLUDE)


def balanced(text, j, open_, close):
    """the index past the group opening at text[j] (open_), or -1"""
    depth = 0
    for k in range(j, len(text)):
        c = text[k]
        if c == open_ and not (open_ == "<" and k > 0 and text[k - 1] == "-"):
            depth += 1
        elif c == close and not (close == ">" and k > 0 and text[k - 1] == "-"):
            depth -= 1
            if depth == 0:
                return k + 1
    return -1


def call_expr(text, j):
    """the transmute call starting at text[j] (its path's last segment): the token, an optional ::<..> turbofish and its
    (..) argument list, over as many lines as the expression takes ('' when it is no call, e.g. a `use`)"""
    m = RE_TM.match(text, j)
    k = m.end()
    while k < len(text) and text[k] in " \n\t":
        k += 1
    if text.startswith("::", k):
        k += 2
        while k < len(text) and text[k] in " \n\t":
            k += 1
        if k < len(text) and text[k] == "<":
            k = balanced(text, k, "<", ">")
            if k < 0:
                return ""
            while k < len(text) and text[k] in " \n\t":
                k += 1
    if k >= len(text) or text[k] != "(":
        return ""
    e = balanced(text, k, "(", ")")
    return text[j:e] if e > 0 else ""


def sites_of_text(text):
    """[(line, item)]: the mcx-transmute sites of one source file's text (memlead's K1 spec, 2026-10-15T00:26:21Z): one per
    transmute / transmute_copy CALL outside crates/_support/mcx where (a) its turbofish or argument names Mcx, (b) its argument
    calls a method named mcx or ending in _mcx, or (c) it initializes a `let` whose annotated type names Mcx, or it is the
    tail of a fn / closure whose return type names Mcx"""
    b = homes_mod.blank(text)
    starts = [0]
    for ln in b.split("\n"):
        starts.append(starts[-1] + len(ln) + 1)
    out = []
    for m in RE_TM.finditer(b):
        call = call_expr(b, m.start())
        if not call:
            continue
        line = next(i for i in range(len(starts) - 1) if starts[i] <= m.start() < starts[i + 1])
        hit = RE_MCX_TYPE.search(call) or RE_MCX_CALL.search(call) or RE_MCX_IDENT.search(call)
        if not hit:
            stmt = b[max(b.rfind(";", 0, m.start()) + 1, m.start() - 400):m.start()]   # this statement, up to the call
            lm = RE_LET.search(stmt)
            if lm and RE_MCX_TYPE.search(lm.group(1)):
                hit = True
        if not hit:
            # (c) the tail of a fn / closure returning Mcx: the nearest `->` above (TAIL_BACK lines) whose return type, up to its
            # `{`, names Mcx, with no `;` / `let` between that `{` and the call
            back = b[starts[max(0, line - TAIL_BACK)]:m.start()]
            a = back.rfind("->")
            if a >= 0:
                br = back.find("{", a)
                if br >= 0 and RE_MCX_TYPE.search(back[a:br]) and ";" not in back[br:] and not re.search(r"\blet\b", back[br:]):
                    hit = True
        if hit:
            out.append((line + 1, "transmute"))
    return out


def git(repo, *args):
    r = subprocess.run(["git", "-C", repo] + list(args), capture_output=True, text=True)
    return r.returncode, r.stdout


def candidates(repo, rev=None):
    """every in-scope .rs file that names a transmute or a pointer cast (git grep: the index or REV, no tree walk)"""
    args = ["grep", "-l", "-E", CANDIDATE] + ([rev] if rev else []) + ["--", "crates/*.rs"]
    _, out = git(repo, *args)
    fs = [x.split(":", 1)[1] if rev and ":" in x else x for x in out.split()]
    return sorted(f for f in fs if in_scope(f))


def read(repo, f, rev=None):
    """a file's text in the worktree (rev None) or at REV (a commit or a tree object); None when absent"""
    if rev is None:
        p = os.path.join(repo, f)
        return open(p, errors="replace").read() if os.path.isfile(p) else None
    rc, out = git(repo, "show", f"{rev}:{f}")
    return out if rc == 0 else None


def read_many(repo, files, rev):
    """{file: text} at REV in one `git cat-file --batch` (a `git show` per file is ~40 ms on the Mac); absent files left out"""
    files = list(files)
    if not files:
        return {}
    r = subprocess.run(["git", "-C", repo, "cat-file", "--batch"], input="".join(f"{rev}:{f}\n" for f in files).encode(),
                       capture_output=True, check=True)
    buf, i, out = r.stdout, 0, {}
    for f in files:
        e = buf.index(b"\n", i)
        head = buf[i:e].split()
        if len(head) < 3 or head[-1] == b"missing":
            i = e + 1
            continue
        n = int(head[2])
        out[f] = buf[e + 1:e + 1 + n].decode(errors="replace")
        i = e + 1 + n + 1
    return out


class RevHomes:
    """homes.Homes over the files at REV (a temp root holding homes.tsv, its rows' files and the files asked for)"""

    def __init__(self, repo, rev, texts):
        import tempfile
        self.tmp = tempfile.mkdtemp(prefix="mcxt-")
        hp = read(repo, HOMES, rev) or ""
        os.makedirs(os.path.dirname(os.path.join(self.tmp, HOMES)), exist_ok=True)
        open(os.path.join(self.tmp, HOMES), "w").write(hp)
        hf = [ln.split("\t")[0] for ln in hp.splitlines() if not ln.startswith("#") and len(ln.split("\t")) >= 3]
        texts = dict(texts)
        texts.update(read_many(repo, [f for f in set(hf) if f not in texts], rev))
        for f, t in texts.items():
            os.makedirs(os.path.dirname(os.path.join(self.tmp, f)), exist_ok=True)
            open(os.path.join(self.tmp, f), "w").write(t)
        self.h = homes_mod.Homes(self.tmp, os.path.join(self.tmp, HOMES))

    def of(self, f, line):
        return self.h.of(f, line)

    def close(self):
        import shutil
        shutil.rmtree(self.tmp, ignore_errors=True)


def scan(repo, files, rev=None):
    """(Counter (file, KIND, item) -> sites, {(file, item): [lines]}) of those files at REV (None: the worktree), homes out"""
    files = [f for f in files if in_scope(f)]
    texts = read_many(repo, files, rev) if rev is not None else {f: read(repo, f) for f in files}
    found = {f: sites_of_text(t) for f, t in texts.items() if t is not None}
    found = {f: v for f, v in found.items() if v}
    if not found:
        return collections.Counter(), collections.defaultdict(list)
    if rev is None:
        hm, close = homes_mod.Homes(repo, os.path.join(repo, HOMES)), None
    else:   # homes only for the files with a site (the spans need their text)
        rh = RevHomes(repo, rev, {f: texts[f] for f in found})
        hm, close = rh, rh.close
    cur, lines = collections.Counter(), collections.defaultdict(list)
    try:
        for f, ss in sorted(found.items()):
            for ln, item in ss:
                if hm.of(f, ln):
                    continue
                cur[(f, KIND, item)] += 1
                lines[(f, item)].append(ln)
    finally:
        if close:
            close()
    return cur, lines


def ledger_name(path):
    return path.replace("/", ".") + ".tsv"


def read_rows(repo):
    d = os.path.join(repo, LEDGER, SUBDIR)
    rows = collections.Counter()
    if not os.path.isdir(d):
        return None
    for n in sorted(os.listdir(d)):
        if n.startswith(".") or n == "README":
            continue
        path = None
        for ln in open(os.path.join(d, n)).read().splitlines():
            c = ln.split("\t")
            if c[0] == "path":
                path = c[1]
            elif len(c) == 3 and c[0] == KIND:
                rows[(path, c[0], c[1])] += int(c[2])
    return rows


def write(repo, cur):
    d = os.path.join(repo, LEDGER, SUBDIR)
    if os.path.isdir(d):
        for n in os.listdir(d):
            os.remove(os.path.join(d, n))
    os.makedirs(d, exist_ok=True)
    by = collections.defaultdict(list)
    for (f, k, it), v in cur.items():
        by[f].append((it, v))
    for f, rs in by.items():
        with open(os.path.join(d, ledger_name(f)), "w") as o:
            o.write(f"path\t{f}\n" + "".join(f"{KIND}\t{it}\t{v}\n" for it, v in sorted(rs)))
    if not by:
        open(os.path.join(d, ".empty"), "w").write("")
    return sum(cur.values()), len(by)


def judge(cur, lines, led, files):
    """the FAIL lines for those files: a site over its row (NEW) or a row over the sites (STALE)"""
    fails = []
    fs = set(files)
    for key in sorted(set(cur) | {k for k in led if k[0] in fs}):
        f, _, it = key
        if f not in fs:
            continue
        c, b = cur.get(key, 0), led.get(key, 0)
        if c > b:
            fails.append(f"alloclint FAIL MCX NEW {f}:{','.join(map(str, sorted(lines[(f, it)])))} {KIND} {it}: {c} site(s), "
                         f"budget {b}: take the handle from an address-stable owner (PinnedContext::handle, McxOwned / SendRoot, a "
                         f"static or leaked context), never transmute a handle of a context held by value "
                         f"(crates/gravity/lint/alloclint/LOWER-A-ROW.md)")
        elif c < b:
            fails.append(f"alloclint FAIL MCX STALE {f} {KIND} {it}: budget {b}, now {c}: lower the row "
                         f"(crates/gravity/lint/ALLOCLINT-DEBT/{SUBDIR}/{ledger_name(f)})")
    return fails


def selftest():
    import tempfile
    t = "\n".join([
        "fn a(cx: &MemoryContext) {",                                                       # 1
        "    let m: Mcx<'static> = unsafe { std::mem::transmute(cx.mcx()) };",             # 2 (b) + (c)
        "    let n = unsafe { core::mem::transmute::<Node<'_>, Node<'mcx>>(p) };",          # 3 not: a Node, an 'mcx lifetime
        "    let s = unsafe { core::mem::transmute::<Mcx<'_>, Mcx<'static>>(m) };",         # 4 (a)
        "    // core::mem::transmute::<Mcx<'_>, Mcx<'static>>(m)",                          # 5 not: a comment
        "    let o = unsafe { transmute::<McxOwned<'_>, McxOwned<'static>>(q) };",          # 6 not: McxOwned is no Mcx
        "    let g: Mcx<'static> = unsafe { transmute(gcx) };",                             # 7 (c) the let's type
        "    let e = unsafe { mem::transmute_copy(&econtext.per_tuple_mcx()) };",           # 8 (b) transmute_copy, a _mcx method
        "    use core::mem::transmute;",                                                     # 9 not: no call
        "    let h: Mcx<'static> =",                                                         # 10
        "        unsafe { core::mem::transmute(scratch.mcx()) };",                            # 11 (b) + (c) over two lines
        "    let s = \"transmute::<Mcx<'_>, Mcx<'static>>\";",                              # 12 not: a string
        "    let u = unsafe {",                                                              # 13
        "        core::mem::transmute::<",                                                   # 14 (a) a call over 3 lines
        "            Mcx<'_>, Mcx<'static>>(m)",                                             # 15
        "    };",                                                                            # 16
        "    let w = unsafe { transmute::<&[u8], &'static [u8]>(b) }; let k = cx.mcx();",   # 17 not: the .mcx() is another call
        "}",                                                                                 # 18
        "fn handle(&self) -> Mcx<'static> {",                                                # 19
        "    unsafe { core::mem::transmute(self.m) }",                                       # 20 (c) the tail of a fn -> Mcx
        "}",                                                                                 # 21
        "fn other(&self) -> Mcx<'static> {",                                                 # 22
        "    let x = 1;",                                                                    # 23
        "    unsafe { core::mem::transmute::<u32, f32>(self.n) };",                          # 24 not: no tail (a `;` between)
        "    self.m",                                                                        # 25
        "}",                                                                                 # 26
        "fn long(&self) -> Mcx<'static> {",                                                  # 27
        "    // SAFETY: one",                                                                # 28
        "    // two",                                                                        # 29
        "    // three",                                                                      # 30
        "    // four",                                                                       # 31
        "    // five",                                                                       # 32
        "    unsafe { core::mem::transmute(self.m) }",                                       # 33 (c) a tail under a 5-line SAFETY
        "}",                                                                                 # 34
        "fn init(mcx: Mcx<'_>) -> State {",                                                  # 35
        "    State { mcx: unsafe { core::mem::transmute(mcx) }, n: 0 }",                     # 36 (b') a bare mcx argument
        "}"])
    got = sites_of_text(t)
    want = [(2, "transmute"), (4, "transmute"), (7, "transmute"), (8, "transmute"), (11, "transmute"), (14, "transmute"),
            (20, "transmute"), (33, "transmute"), (36, "transmute")]
    assert got == want, got
    with tempfile.TemporaryDirectory() as r:
        f = "crates/backend/x/src/lib.rs"
        os.makedirs(os.path.join(r, "crates/backend/x/src"))
        open(os.path.join(r, f), "w").write(t + "\nfn pinned(p: &PinnedContext) {\n    let h = unsafe { core::mem::transmute::<Mcx<'_>, Mcx<'static>>(p.mcx()) };\n}\n")
        os.makedirs(os.path.join(r, "crates/_support/mcx/src"))
        open(os.path.join(r, "crates/_support/mcx/src/lib.rs"), "w").write(t)
        cur, lines = scan(r, [f, "crates/_support/mcx/src/lib.rs"])
        assert cur == {(f, KIND, "transmute"): 10}, cur   # the mcx crate is out of scope
        os.makedirs(os.path.join(r, os.path.dirname(HOMES)))
        open(os.path.join(r, HOMES), "w").write(f"{f}\tpinned\tH3\tPinnedContext::handle\n")
        cur, lines = scan(r, [f])
        assert cur[(f, KIND, "transmute")] == 9 and lines[(f, "transmute")] == [2, 4, 7, 8, 11, 14, 20, 33, 36], (cur, lines)   # a home: tracked
        n, nf = write(r, cur)
        assert n == 9 and nf == 1 and read_rows(r) == cur
        assert judge(cur, lines, read_rows(r), [f]) == []
        cur2 = collections.Counter(cur)
        cur2[(f, KIND, "transmute")] += 1
        lines[(f, "transmute")].append(30)
        fl = judge(cur2, lines, read_rows(r), [f])
        assert len(fl) == 1 and fl[0].startswith("alloclint FAIL MCX NEW " + f + ":2,4,7,8,11,14,20,30,33,36 mcx-transmute transmute: 10 site(s), budget 9"), fl
        cur2[(f, KIND, "transmute")] -= 2
        fl = judge(cur2, lines, read_rows(r), [f])
        assert len(fl) == 1 and "FAIL MCX STALE " + f + " mcx-transmute transmute: budget 9, now 8" in fl[0], fl
        assert judge(cur2, lines, read_rows(r), ["crates/other.rs"]) == [], "only the files asked for"
    print("mcxt selftest: ok")
    return 0


def main():
    import argparse
    ap = argparse.ArgumentParser()
    ap.add_argument("cmd", choices=["scan", "write", "check", "selftest"])
    ap.add_argument("--repo", default=".")
    ap.add_argument("--rev")
    ap.add_argument("files", nargs="*")
    a = ap.parse_args()
    if a.cmd == "selftest":
        return selftest()
    files = a.files or candidates(a.repo, a.rev if a.cmd == "scan" else None)
    cur, lines = scan(a.repo, files, a.rev if a.cmd == "scan" else None)
    if a.cmd == "scan":
        for (f, it), ls in sorted(lines.items()):
            for ln in ls:
                print(f"{f}:{ln}\t{it}")
        by = collections.Counter()
        for (f, _, it), v in cur.items():
            by["/".join(f.split("/")[1:3])] += v
        print(f"mcxt: {sum(cur.values())} site(s) in {len({k[0] for k in cur})} file(s) of {len(files)} scanned "
              f"; by crate area: {dict(by.most_common())}")
        return 0
    if a.cmd == "write":
        n, nf = write(a.repo, cur)
        print(f"mcxt: wrote {n} site(s) in {nf} file(s) to {LEDGER}/{SUBDIR}")
        return 0
    led = read_rows(a.repo)
    if led is None:
        print(f"mcxt: CANNOT JUDGE: no {LEDGER}/{SUBDIR}")
        return 2
    fails = judge(cur, lines, led, sorted(set(files) | {k[0] for k in led}))
    for x in fails:
        print(x)
    print(f"mcxt: {'FAIL' if fails else 'PASS'}: {sum(cur.values())} site(s) now, {sum(led.values())} budgeted")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
