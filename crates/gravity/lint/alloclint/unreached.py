#!/usr/bin/env python3
"""UNREACHED (TD-197 amendment; USER RULING via memacct 2026-10-15 ~18:5xZ task A (3), GO 2026-10-15T20:3xZ): a function no
server path reaches allocates nothing in the server, so its findings are a NAMED class (no row, outside NEW and the debt
total), never debt, as long as a mechanical proof holds on the tree.

unreached.tsv lists the functions (`<file>\t<function>\t<note>`, function = `name` or `Type::name`, the last segment is the
name searched). THE PROOF (ruling (3): the name appearing anywhere outside test code reaches it): `git grep -w <name>` over the
tree's .rs files, a hit EXCLUDED only when it is
  - a `fn <name>` definition (this one or another function of the same name; a CALL of that other one still counts),
  - inside a comment (a name in a string literal counts: registries and callback tables reach by name),
  - in a file under tests/, examples/ or benches/, or a package's build.rs,
  - inside an item under `#[cfg(test)]` / `#[cfg(all(test, ..))]` (a `mod x;` so declared excludes x.rs / x/ too),
  - in a package outside the server's normal-dependency closure (from the package building the `postgres` binary, over
    [dependencies] and [target.*.dependencies], optional ones included; dev- and build-dependencies are out).
Any other hit = REACHED: the row fails (ratchet.py base FAIL UNREACHED), and dropping it turns its sites into debt rows,
which FAIL ADDS (a landing that makes test code server-reachable converts those sites). A method called by a common name
(push, compile, load, walk) cannot pass; its sites go to CONVERT (memacct's ruling).

FILE ROWS (memacct 2026-10-16T05:0xZ, the same class at file granularity: `<file>\t*\t<note>`, the note naming FILE_PROOF and
its exclusions): every function of a module file at once, for a module only test code names (a plan fixture reader whose
entry points have common names). THE FILE PROOF (prove_file), re-run on EVERY lint run (ratchet.py check / write and base):
  - the file is a module file of a package in the server closure (src/<path>.rs or src/<path>/mod.rs, not lib.rs / main.rs /
    src/bin), and its parent file declares it (`mod <name>;`);
  - no PATH names the module outside test code: the module's name followed or preceded by `::`, or inside a `use`
    declaration, in any .rs file of a package in the server closure (comments wiped; the test exclusions above; the parent's
    `mod <name>;` line and the file itself excluded); and no code names the file (`#[path]`, include!);
  - nothing in the file reaches it without a path: no macro_rules! / #[macro_export], no #[no_mangle] / #[export_name] /
    #[link_section] / #[used] / extern "..", no ctor / distributed_slice / inventory registration, and every impl is for a
    type the file itself defines.
A file row whose proof fails on a tree: its sites are debt on that tree (FAIL ADDS / NEW), and the lint prints why.

    unreached.py prove --repo R [--rev REV] (FILE:LINE | FILE::FN)...   the proof for sites (their enclosing fn) or functions
    unreached.py prove --repo R [--rev REV] FILE::*                     the file proof
    unreached.py check --repo R [--rev REV]                             re-prove every unreached.tsv row (exit 1 on a REACHED row)
    unreached.py selftest
REV None = the worktree (git grep over the index's files, read from disk).
"""
import collections
import os
import re
import subprocess
import sys
import tempfile
import tomllib

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import homes  # noqa: E402

LIST = "crates/gravity/lint/alloclint/unreached.tsv"
SERVER_BIN = "postgres"
FILE_PROOF = "file-proof v1"   # a FILE row's note names this (and the exclusions): the proof it was written against
RE_CFG_TEST = re.compile(r"#\s*\[\s*cfg\s*\(\s*(?:test\s*\)|all\s*\((?:[^()]|\([^()]*\))*\btest\b(?:[^()]|\([^()]*\))*\)\s*\))\s*\]")


def git(repo, *a, inp=None):
    r = subprocess.run(["git", "-C", repo] + list(a), capture_output=True, text=True, input=inp)
    return r.returncode, r.stdout


def read_rows(text):
    """[(file, function, note)] of an unreached.tsv text"""
    out = []
    for ln in text.splitlines():
        if not ln.strip() or ln.startswith("#"):
            continue
        c = ln.split("\t")
        if len(c) >= 2 and c[0].strip() and c[1].strip():
            out.append((c[0].strip(), c[1].strip(), c[2].strip() if len(c) > 2 else ""))
    return out


class Tree:
    """a tree's files: REV (git objects) or the worktree (rev None)"""

    def __init__(self, repo, rev=None):
        self.repo, self.rev, self.cache = repo, rev, {}
        self.cat = None   # REV's reads: one `git cat-file --batch` process (batch_text)
        self.batch = os.environ.get("ALLOCLINT_BATCH_READ", "1") != "0"   # 0: one `git show` per file (the old path)

    def files(self):
        rc, out = git(self.repo, "ls-tree", "-r", "--name-only", self.rev) if self.rev else git(self.repo, "ls-files")
        return out.split() if rc == 0 else []

    def batch_text(self, p):
        """REV's blob p through one long-lived `git cat-file --batch`: its text, "" when REV has no such path, None when only
        `git show` can answer (not a blob, not UTF-8, a path with a newline, a batch error). The proof reads ~3,800 files per
        run (every Cargo.toml, every .rs file of a package with a hit); one `git show` process each took ~150 s on the Mac
        (S0) and ~30 s on a box. The text equals `git show`'s: the blob's bytes as UTF-8, universal newlines."""
        if "\n" in p:
            return None
        try:
            if self.cat is None:
                self.cat = subprocess.Popen(["git", "-C", self.repo, "cat-file", "--batch"], stdin=subprocess.PIPE,
                                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
            self.cat.stdin.write(f"{self.rev}:{p}\n".encode())
            self.cat.stdin.flush()
            head = self.cat.stdout.readline().decode("utf-8", "replace").rstrip("\n")
            if head.endswith(" missing"):
                return ""
            w = head.split(" ")
            if len(w) != 3 or not w[2].isdigit():
                raise ValueError(head[:80])
            data = self.cat.stdout.read(int(w[2]))
            if len(data) != int(w[2]) or self.cat.stdout.read(1) != b"\n":
                raise ValueError("short read")
            if w[1] != "blob":
                return None
            return data.decode("utf-8").replace("\r\n", "\n").replace("\r", "\n")
        except UnicodeDecodeError:
            return None
        except (OSError, ValueError):
            self.close()
            return None

    def close(self):
        if self.cat is not None:
            try:
                self.cat.stdin.close()
                self.cat.wait(timeout=10)
            except (OSError, subprocess.TimeoutExpired):
                self.cat.kill()
            self.cat = None

    def text(self, p):
        if p not in self.cache:
            if self.rev:
                t = self.batch_text(p) if self.batch else None
                if t is None:
                    rc, t = git(self.repo, "show", f"{self.rev}:{p}")
                    t = t if rc == 0 else ""
                self.cache[p] = t
            else:
                full = os.path.join(self.repo, p)
                self.cache[p] = open(full, errors="replace").read() if os.path.exists(full) else ""
        return self.cache[p]

    def grep(self, names):
        """{path: set(line numbers)} of the lines naming any of `names` as a word (.rs files)"""
        args = ["grep", "-n", "-w", "-I"] + sum((["-e", n] for n in sorted(set(names))), [])
        args += ([self.rev] if self.rev else []) + ["--", "*.rs"]
        rc, out = git(self.repo, *args)
        hits = collections.defaultdict(set)
        for ln in out.splitlines():
            if self.rev and ln.startswith(self.rev + ":"):
                ln = ln[len(self.rev) + 1:]
            p, _, rest = ln.partition(":")
            num, _, _ = rest.partition(":")
            if num.isdigit():
                hits[p].add(int(num))
        return hits


def packages(tree):
    """{package dir: (name, toml dict)} of every Cargo.toml with a [package]"""
    out = {}
    for p in tree.files():
        if p == "Cargo.toml" or p.endswith("/Cargo.toml"):
            try:
                d = tomllib.loads(tree.text(p))
            except tomllib.TOMLDecodeError:
                continue
            if "package" in d:
                out[os.path.dirname(p)] = (d["package"].get("name", ""), d)
    return out


def server_closure(tree, pkgs=None):
    """the package dirs the `postgres` binary's package reaches over its normal dependencies"""
    pkgs = packages(tree) if pkgs is None else pkgs
    try:
        root_toml = tomllib.loads(tree.text("Cargo.toml"))
    except tomllib.TOMLDecodeError:
        root_toml = {}
    ws = root_toml.get("workspace", {}).get("dependencies", {})
    by_name = {nm: d for d, (nm, _) in pkgs.items()}

    def deps_of(d, t):
        tables = [t.get("dependencies", {})] + [v.get("dependencies", {}) for v in t.get("target", {}).values() if isinstance(v, dict)]
        out = []
        for tab in tables:
            for k, v in tab.items():
                v = v if isinstance(v, dict) else {}
                if v.get("workspace"):
                    w = ws.get(k, {})
                    w = w if isinstance(w, dict) else {}
                    if "path" in w:
                        out.append(os.path.normpath(w["path"]))
                    else:
                        out.append(by_name.get(w.get("package", k)))
                elif "path" in v:
                    out.append(os.path.normpath(os.path.join(d, v["path"])))
                else:
                    out.append(by_name.get(v.get("package", k)))
        return [x for x in out if x in pkgs]
    roots = [d for d, (_, t) in pkgs.items() if any(b.get("name") == SERVER_BIN for b in t.get("bin", []))]
    seen, todo = set(roots), list(roots)
    while todo:
        d = todo.pop()
        for x in deps_of(d, pkgs[d][1]):
            if x not in seen:
                seen.add(x)
                todo.append(x)
    return seen, roots


def mod_files(decl, name):
    """the file and directory a `mod name;` in file `decl` loads (x.rs and x/ beside a lib.rs / main.rs / mod.rs, else under
    the declaring file's own directory)"""
    d, stem = os.path.dirname(decl), os.path.splitext(os.path.basename(decl))[0]
    base = d if stem in ("lib", "main", "mod") else os.path.join(d, stem)
    return os.path.join(base, name + ".rs"), os.path.join(base, name)


def test_spans(text, path):
    """([(first line, last line)] of the items under #[cfg(test)], [(file, dir)] of the test modules they load)"""
    b = homes.blank(text)
    line_of = lambda k: b.count("\n", 0, k) + 1  # noqa: E731
    spans, files = [], []
    for m in RE_CFG_TEST.finditer(b):
        k = m.end()
        # skip further attributes before the item
        while True:
            mm = re.match(r"\s*#\s*\[", b[k:])
            if not mm:
                break
            depth, j = 1, k + mm.end()
            while j < len(b) and depth:
                depth += {"[": 1, "]": -1}.get(b[j], 0)
                j += 1
            k = j
        semi, brace = b.find(";", k), b.find("{", k)
        if semi >= 0 and (brace < 0 or semi < brace):
            mm = re.match(r"\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;", b[k:semi + 1])
            if mm:
                files.append(mod_files(path, mm.group(1)))
            spans.append((line_of(m.start()), line_of(semi)))
            continue
        if brace < 0:
            continue
        depth, j = 0, brace
        while j < len(b):
            if b[j] == "{":
                depth += 1
            elif b[j] == "}":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        spans.append((line_of(m.start()), line_of(j)))
    return spans, files


RE_PATH_FREE = [(re.compile(r), what) for r, what in (
    (r"\bmacro_rules\s*!", "macro_rules!"), (r"#\s*\[\s*macro_export", "#[macro_export]"), (r"\bno_mangle\b", "#[no_mangle]"),
    (r"\bexport_name\b", "#[export_name]"), (r"\blink_section\b", "#[link_section]"), (r"#\s*\[\s*used\b", "#[used]"),
    (r"\bextern\s*\"", "extern \"..\""), (r"#\s*\[\s*ctor\b", "#[ctor]"), (r"\bdistributed_slice\b", "distributed_slice"),
    (r"\binventory\s*::\s*submit\b", "inventory::submit"))]


def module_of(path, pdir):
    """(module name, parent file candidates) of a module file of the package at pdir, or (None, why)"""
    rel = os.path.relpath(path, pdir)
    parts = rel.split("/")
    if parts[0] != "src" or len(parts) < 2 or not parts[-1].endswith(".rs") or parts[1] == "bin":
        return None, "not a module file under the package's src/ (src/bin excluded)"
    mods = parts[1:-1] + [parts[-1][:-3]]
    if mods[-1] == "mod":
        mods = mods[:-1]
    if not mods or mods == ["lib"] or mods == ["main"]:
        return None, "a crate root (lib.rs / main.rs), not a module file"
    up = mods[:-1]
    if up:
        parents = [os.path.join(pdir, "src", *up) + ".rs", os.path.join(pdir, "src", *up, "mod.rs")]
    else:
        parents = [os.path.join(pdir, "src", "lib.rs"), os.path.join(pdir, "src", "main.rs")]
    return mods[-1], parents


def impl_violations(b):
    """[(line, header)] of the impls in blanked text `b` whose self type the text does not define"""
    local = set(re.findall(r"\b(?:struct|enum|union|trait|type)\s+([A-Za-z_]\w*)", b))
    out = []
    for m in re.finditer(r"\bimpl\b", b):
        j = b.find("{", m.end())
        if j < 0:
            continue
        h = b[m.end():j]
        k, depth = 0, 0
        hs = h.lstrip()
        if hs.startswith("<"):   # the impl's own generic parameters
            off = len(h) - len(hs)
            for k in range(len(hs)):
                depth += {"<": 1, ">": -1}.get(hs[k], 0)
                if depth == 0:
                    break
            h = h[off + k + 1:]
        # the self type: after a ` for ` at angle depth 0, else the whole header (an inherent impl); `where` cut
        depth, cut, i = 0, None, 0
        while i < len(h):
            ch = h[i]
            depth += {"<": 1, ">": -1}.get(ch, 0)
            if depth == 0 and re.match(r"\bfor\b", h[i:]) and (i == 0 or not (h[i - 1].isalnum() or h[i - 1] == "_")):
                cut = i + 3
            i += 1
        self_t = h[cut:] if cut is not None else h
        self_t = re.split(r"\bwhere\b", self_t)[0].strip()
        base = re.sub(r"^(?:&\s*(?:'\w+\s*)?(?:mut\s+)?|dyn\s+|\*\s*(?:const|mut)\s+)+", "", self_t)
        mm = re.match(r"((?:[A-Za-z_]\w*\s*::\s*)*)([A-Za-z_]\w*)", base)
        if not mm or mm.group(1) or mm.group(2) not in local:
            out.append((b.count("\n", 0, m.start()) + 1, " ".join(("impl" + h).split())[:100]))
    return out


def prove_file(tree, f, pkgs=None, closure=None):
    """[reasons the file proof fails] for module file f ([] = proven): FILE ROWS in the module doc"""
    pkgs = packages(tree) if pkgs is None else pkgs
    if closure is None:
        closure, roots = server_closure(tree, pkgs)
        if not roots:
            return ["no package builds the %s binary: cannot prove" % SERVER_BIN]
    pdirs = sorted(pkgs, key=len, reverse=True)
    pkg = lambda p: next((d for d in pdirs if p.startswith(d + "/")), None)  # noqa: E731
    d0 = pkg(f)
    text = tree.text(f)
    if d0 is None or not text:
        return [f"{f}: no such file in a package"]
    if d0 not in closure:
        return [f"{f}: its package is outside the server closure (no file row needed)"]
    m, parents = module_of(f, d0)
    if m is None:
        return [f"{f}: {parents}"]
    rx_decl = re.compile(r"(?:^|\n)[ \t]*(?:pub(?:\([^)]*\))?\s+)?mod\s+" + re.escape(m) + r"\s*;")
    decl = None
    for pf in parents:
        bt = homes.blank(tree.text(pf))
        mm = rx_decl.search(bt)
        if mm:
            decl = (pf, bt.count("\n", 0, mm.end()) + 1)
            break
    if decl is None:
        return [f"{f}: no `mod {m};` in {' / '.join(parents)} (a #[path] or inline module): cannot prove"]
    reasons = []
    b = homes.blank(text)
    for rx, what in RE_PATH_FREE:
        for mm in rx.finditer(b):
            reasons.append(f"{f}:{b.count(chr(10), 0, mm.start()) + 1}: {what} (reaches without the module's path)")
    for ln, h in impl_violations(b):
        reasons.append(f"{f}:{ln}: `{h}` is for a type the file does not define (reaches without the module's path)")
    # paths naming the module, and code naming the file, outside test code
    fname = os.path.basename(f)
    hits = tree.grep([m, fname[:-3]])
    declared = {}
    by_pkg = collections.defaultdict(list)
    for q in tree.files():
        if q.endswith(".rs"):
            dq = pkg(q)
            if dq is not None:
                by_pkg[dq].append(q)
    rx_m = re.compile(r"(?<![A-Za-z0-9_])" + re.escape(m) + r"(?![A-Za-z0-9_])")
    rx_f = re.compile(re.escape(fname))
    for p in sorted(hits):
        if p == f:
            continue
        parts = p.split("/")
        d = pkg(p)
        if d is None or d not in closure:
            continue
        if any(x in ("tests", "examples", "benches") for x in parts[:-1]) or p == os.path.join(d, "build.rs"):
            continue
        if d not in declared:
            declared[d] = []
            for q in by_pkg[d]:
                tq = tree.text(q)
                if "cfg" in tq and "mod " in tq:
                    declared[d] += test_spans(tq, q)[1]
        if any(p == fl or p.startswith(dd + "/") for fl, dd in declared[d]):
            continue
        t = tree.text(p)
        lines = t.splitlines()
        spans = test_spans(t, p)[0]
        cb = homes.blank(t, strings=False)
        uses = [(u.start(), u.end()) for u in re.finditer(r"(?<![A-Za-z0-9_])use\b[^;]*;", cb)]
        starts = [0] + [i + 1 for i, ch in enumerate(cb) if ch == "\n"]
        for ln in sorted(hits[p]):
            if ln - 1 >= len(starts) or any(a <= ln <= z for a, z in spans):
                continue
            if decl == (p, ln):
                continue   # the parent's `mod name;`
            a0 = starts[ln - 1]
            code = cb[a0:starts[ln] - 1] if ln < len(starts) else cb[a0:]
            why = None
            for mm in rx_m.finditer(code):
                at = a0 + mm.start()
                if re.match(r"\s*::", code[mm.end():]) or re.search(r"::\s*$", code[:mm.start()]):
                    why = "a path"
                elif any(u0 <= at < u1 for u0, u1 in uses):
                    why = "a use"
                if why:
                    break
            if why is None and rx_f.search(code):
                why = "the file's name"
            if why:
                reasons.append(f"{p}:{ln}: {lines[ln - 1].strip()[:140]} ({why})")
    return reasons


def prove(tree, fns):
    """{(file, function): [reaching hits 'path:line: text', ...]} ([] = the proof holds) for [(file, function)]; a function
    `*` is a FILE row (prove_file)"""
    pkgs = packages(tree)
    closure, roots = server_closure(tree, pkgs)
    if not roots:
        return {f: ["no package builds the %s binary: cannot prove" % SERVER_BIN] for f in fns}
    file_rows = [f for f in fns if f[1] == "*"]
    if file_rows:
        out = {f: prove_file(tree, f[0], pkgs, closure) for f in file_rows}
        rest = [f for f in fns if f[1] != "*"]
        if rest:
            out.update(prove(tree, rest))
        return out
    pdirs = sorted(pkgs, key=len, reverse=True)
    names = {f: f[1].split("::")[-1] for f in fns}
    hits = tree.grep(names.values())
    tspans, pkg_of, out = {}, {}, {}

    def pkg(p):
        if p not in pkg_of:
            pkg_of[p] = next((d for d in pdirs if p.startswith(d + "/")), None)
        return pkg_of[p]
    # the files a #[cfg(test)] `mod x;` loads, per package of a hit (every .rs file of those packages is read once)
    declared = {}
    all_files = tree.files()
    by_pkg = collections.defaultdict(list)
    for q in all_files:
        if q.endswith(".rs"):
            dq = pkg(q)
            if dq is not None:
                by_pkg[dq].append(q)
    for d in {pkg(p) for p in hits} - {None}:
        declared[d] = []
        for q in by_pkg[d]:
            t = tree.text(q)
            if "cfg" in t and "mod " in t:
                declared[d] += test_spans(t, q)[1]
    for f in fns:
        nm = names[f]
        rx = re.compile(r"(?<![A-Za-z0-9_])" + re.escape(nm) + r"(?![A-Za-z0-9_])")
        reach = []
        for p in sorted(hits):
            lines = tree.text(p).splitlines()
            if not any(nm in lines[ln - 1] for ln in hits[p] if ln - 1 < len(lines)):
                continue
            parts = p.split("/")
            d = pkg(p)
            if any(x in ("tests", "examples", "benches") for x in parts[:-1]) or (d is not None and p == os.path.join(d, "build.rs")):
                continue
            if d is None or d not in closure:
                continue
            if any(p == fl or p.startswith(dd + "/") for fl, dd in declared.get(d, [])):
                continue
            t = tree.text(p)
            if p not in tspans:
                tspans[p] = test_spans(t, p)[0]
            cb = homes.blank(t, strings=False).splitlines()
            for ln in sorted(hits[p]):
                if ln - 1 >= len(lines) or any(a <= ln <= b for a, b in tspans[p]):
                    continue
                code = cb[ln - 1] if ln - 1 < len(cb) else ""
                for m in rx.finditer(code):
                    if re.search(r"\bfn\s+$", code[:m.start()]):
                        continue   # a definition
                    reach.append(f"{p}:{ln}: {lines[ln - 1].strip()[:140]}")
                    break
        out[f] = reach
    return out


def enclosing(tree, f, line):
    """the function (Type::name or name) whose body holds line `line` of f, the innermost; None outside every fn"""
    best = None
    for row in homes.fn_spans(tree.text(f)):
        name, selfty, a, b = row[:4]
        if a <= line <= b and (best is None or b - a < best[1] - best[0]):
            best = (a, b, (selfty + "::" if selfty else "") + name)
    return best[2] if best else None


def main(argv):
    if argv[:1] == ["selftest"]:
        return selftest()
    if not argv or argv[0] not in ("prove", "check"):
        print(__doc__)
        return 2
    cmd, args = argv[0], argv[1:]
    repo, rev, items = ".", None, []
    i = 0
    while i < len(args):
        if args[i] == "--repo":
            repo, i = args[i + 1], i + 2
        elif args[i] == "--rev":
            rev, i = args[i + 1], i + 2
        else:
            items.append(args[i])
            i += 1
    tree = Tree(repo, rev)
    if cmd == "check":
        rows = read_rows(tree.text(LIST))
        res = prove(tree, [(f, fn) for f, fn, _ in rows])
        for f, fn, note in rows:
            if fn == "*" and FILE_PROOF not in note:
                res[(f, fn)] = [f"the row's note does not name {FILE_PROOF!r} (the proof it was written against) and its exclusions"] + res[(f, fn)]
        bad = 0
        for (f, fn), hs in sorted(res.items()):
            if hs:
                bad += 1
                print(f"alloclint FAIL UNREACHED {f} {fn}: reached ({len(hs)} hit(s)): " + " | ".join(hs[:3]))
        print(f"alloclint unreached: {len(rows) - bad} of {len(rows)} row(s) proven")
        return 1 if bad else 0
    fns, site_of = [], collections.defaultdict(list)
    for it in items:
        m = re.match(r"(.+\.rs):(\d+)$", it)
        if m:
            fn = enclosing(tree, m.group(1), int(m.group(2)))
            if fn is None:
                print(f"{it}\tNO-FN\t-")
                continue
            key = (m.group(1), fn)
            site_of[key].append(it)
        else:
            f, _, fn = it.partition("::")
            key = (f, fn)
        if key not in fns:
            fns.append(key)
    res = prove(tree, fns)
    for key in fns:
        hs = res[key]
        print("%s\t%s\t%s\t%s\t%s" % (key[0], key[1], "PROVEN" if not hs else "REACHED", len(hs), " | ".join(hs[:3]) or "-"))
    return 0


def selftest():
    with tempfile.TemporaryDirectory() as r:
        def put(p, t):
            os.makedirs(os.path.dirname(os.path.join(r, p)) or r, exist_ok=True)
            open(os.path.join(r, p), "w").write(t)
        put("Cargo.toml", '[workspace]\nmembers = ["crates/*"]\n')
        put("crates/srv/Cargo.toml", '[package]\nname = "srv"\n[[bin]]\nname = "postgres"\npath = "src/main.rs"\n'
            '[dependencies]\nlib1 = { path = "../lib1" }\n[dev-dependencies]\ntk = { path = "../tk" }\n')
        put("crates/srv/src/main.rs", "use lib1::{\n    used,\n    fixt5,\n};\nfn main() { used(); }\n")
        put("crates/lib1/Cargo.toml", '[package]\nname = "lib1"\n')
        put("crates/lib1/src/lib.rs",
            "pub fn used() {}\n"
            "pub fn only_tests() { let v = vec![1]; }\n"                  # 2: called from tests/, a cfg(test) mod, tk, a comment
            "pub fn by_string() {}\n"                                     # 3: named in a string (a registry)
            "pub fn by_value() {}\n"                                      # 4: passed as a value
            "#[cfg(test)]\nmod t {\n    fn x() { super::only_tests(); }\n}\n"
            "#[cfg(test)]\nmod tfile;\n"
            "// only_tests() is called by the tests\n"
            "pub static REG: &[(&str, fn())] = &[(\"by_string\", used), (\"x\", by_value)];\n"
            "pub fn other() { only_shadow(); }\n"
            "pub fn only_shadow() {}\n"
            # FILE rows: fixt (only tests name it), fixt2 (one non-test path: the born-red case), fixt3 (a foreign impl),
            # fixt4 (a macro), fixt5 (a use in another package), fixt7 (a #[path] names the file); fixt6 has no `mod`
            "pub mod fixt;\npub mod fixt2;\npub mod fixt3;\npub mod fixt4;\npub mod fixt5;\npub mod fixt7;\n"
            "pub struct Other { pub fixt: u8 }\n"                       # a field named like the module: no path
            "pub fn live2() -> u8 { fixt2::load(); 0 }\n"
            "#[cfg(test)]\nmod t2 {\n    fn z() { crate::fixt::load(); super::fixt3::load(); }\n}\n"
            "#[path = \"fixt7.rs\"]\nmod seven;\n")
        fx = "pub fn load() { let v = vec![1]; helper(); }\nfn helper() {}\npub struct Fx;\nimpl Fx {\n    fn a(&self) {}\n}\n"
        put("crates/lib1/src/fixt.rs", fx)
        put("crates/lib1/src/fixt2.rs", fx)
        put("crates/lib1/src/fixt3.rs", fx + "impl crate::Other {\n    fn b(&self) {}\n}\n")
        put("crates/lib1/src/fixt4.rs", fx + "macro_rules! m { () => {} }\n")
        put("crates/lib1/src/fixt5.rs", fx)
        put("crates/lib1/src/fixt6.rs", fx)
        put("crates/lib1/src/fixt7.rs", fx)
        put("crates/lib1/tests/b.rs", "use lib1::fixt::{load, Fx};\nfn t() { load(); lib1::fixt2::load(); }\n")
        put("crates/lib1/src/tfile.rs", "fn y() { crate::only_tests(); crate::only_shadow(); }\n")
        put("crates/lib1/tests/a.rs", "fn t() { lib1::only_tests(); }\n")
        put("crates/tk/Cargo.toml", '[package]\nname = "tk"\n')
        put("crates/tk/src/lib.rs", "pub fn k() { lib1::only_tests(); }\n")
        open(os.path.join(r, "crlf.txt"), "wb").write(b"a\r\nb\rc\n\xc3\xa9\n")
        subprocess.run(["git", "init", "-q", r], check=True)
        subprocess.run(["git", "-C", r, "add", "-A"], check=True)
        subprocess.run(["git", "-C", r, "-c", "user.email=x@y", "-c", "user.name=x", "commit", "-qm", "s"], check=True)
        # batch reads (ALLOCLINT_BATCH_READ, default on) give `git show`'s text; a tree or a missing path: show's answer
        tb = Tree(r, "HEAD")
        for p in ("Cargo.toml", "crates/lib1/src/lib.rs", "crlf.txt", "no/such.rs", "crates", "crates/lib1/src/fixt.rs"):
            rc, want = git(r, "show", f"HEAD:{p}")
            want = want if rc == 0 else ""
            got = tb.text(p)
            assert got == want, (p, got[:60], want[:60])
            assert tb.batch_text(p) == (None if p == "crates" else want), p
        assert tb.cat is not None and tb.text("crlf.txt") == "a\nb\nc\n\u00e9\n"
        tb.close()
        os.environ["ALLOCLINT_BATCH_READ"] = "0"
        try:
            to = Tree(r, "HEAD")
            assert to.text("crlf.txt") == "a\nb\nc\n\u00e9\n" and to.text("no/such.rs") == "" and to.cat is None
        finally:
            del os.environ["ALLOCLINT_BATCH_READ"]
        for rev in (None, "HEAD"):
            t = Tree(r, rev)
            closure, roots = server_closure(t)
            assert roots == ["crates/srv"] and closure == {"crates/srv", "crates/lib1"}, (closure, roots)
            f = "crates/lib1/src/lib.rs"
            res = prove(t, [(f, "only_tests"), (f, "by_string"), (f, "by_value"), (f, "used"), (f, "only_shadow")])
            assert res[(f, "only_tests")] == [], res
            assert res[(f, "by_string")] and "REG" in res[(f, "by_string")][0], res      # a string names it: reached
            assert res[(f, "by_value")] and "REG" in res[(f, "by_value")][0], res
            assert any("main.rs" in h for h in res[(f, "used")]), res
            # FILE rows
            L = "crates/lib1/src/"
            rf = prove(t, [(L + x + ".rs", "*") for x in ("fixt", "fixt2", "fixt3", "fixt4", "fixt5", "fixt6", "fixt7")] + [(f, "used")])
            assert rf[(L + "fixt.rs", "*")] == [], rf[(L + "fixt.rs", "*")]
            assert len(rf[(L + "fixt2.rs", "*")]) == 1 and "live2" in rf[(L + "fixt2.rs", "*")][0] and "(a path)" in rf[(L + "fixt2.rs", "*")][0], rf
            assert len(rf[(L + "fixt3.rs", "*")]) == 1 and "impl crate::Other" in rf[(L + "fixt3.rs", "*")][0], rf
            assert len(rf[(L + "fixt4.rs", "*")]) == 1 and "macro_rules!" in rf[(L + "fixt4.rs", "*")][0], rf
            assert len(rf[(L + "fixt5.rs", "*")]) == 1 and "main.rs:3" in rf[(L + "fixt5.rs", "*")][0] and "(a use)" in rf[(L + "fixt5.rs", "*")][0], rf
            assert rf[(L + "fixt6.rs", "*")] and "no `mod fixt6;`" in rf[(L + "fixt6.rs", "*")][0], rf
            assert any("the file's name" in h for h in rf[(L + "fixt7.rs", "*")]), rf
            assert any("main.rs" in h for h in rf[(f, "used")]), rf      # function rows beside file rows
            assert prove_file(t, f) and "crate root" in prove_file(t, f)[0], prove_file(t, f)
            assert len(res[(f, "only_shadow")]) == 1 and "fn other()" in res[(f, "only_shadow")][0], res   # other() calls it
            assert enclosing(t, f, 2) == "only_tests", enclosing(t, f, 2)
        # check: a row reached fails
        put(LIST, "# unreached\ncrates/lib1/src/lib.rs\tonly_tests\tx\ncrates/lib1/src/lib.rs\tused\ty\n")
        import io
        import contextlib
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            rc = main(["check", "--repo", r])
        assert rc == 1 and "FAIL UNREACHED crates/lib1/src/lib.rs used" in buf.getvalue() and "1 of 2 row(s) proven" in buf.getvalue(), buf.getvalue()
        # file rows in check: proven with the note, the born-red file reached, a note without FILE_PROOF fails
        put(LIST, f"# unreached\ncrates/lib1/src/fixt.rs\t*\t{FILE_PROOF}: x\ncrates/lib1/src/fixt2.rs\t*\t{FILE_PROOF}: x\n"
                  "crates/lib1/src/fixt5.rs\t*\tno version\n")
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            rc = main(["check", "--repo", r])
        o = buf.getvalue()
        assert rc == 1 and "1 of 3 row(s) proven" in o and "fixt2.rs *: reached" in o and "does not name" in o, o
    print("unreached selftest: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
