#!/usr/bin/env python3
"""RT's scope seed (C7 coverage E2): the thread-locals of rule RT's scope (lint/rules/rt_thread_state.py, every source
under crates/ outside tests/) with no row in lint/THREAD-STATE, TLS-DEBT or SCOPE-DEBT, each written as one of:

- a registry row <path> <static> unittest test test <proof>, when the static's file is a unit-test module: its parent
  module's file declares it at the top level as `mod <name>;` and every such declaration carries #[cfg(test)], or the
  parent's file is itself such a module (the proof names the declaring file);
- else a SCOPE-DEBT row <path> <static> isojoin <what is missing>.

    rt_scope_seed.py [--root DIR] [--write]
    rt_scope_seed.py --selftest

Without --write it prints the counts and the files it could not prove. A parent found through #[path], an inline
module or a cfg other than exactly #[cfg(test)] (`cfg(any(test, feature = ..))`) proves nothing: its statics are debt."""
import argparse
import collections
import os
import re
import shutil
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "rules"))
import common  # noqa: E402
import rt_thread_state as rt  # noqa: E402

OWNER, LANE, MISSING = "unittest", "isojoin", "owner, reset, carry"
_CFG = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")


def _code(text):
    """`text` with comments blanked and strings emptied (a char literal holding a bracket becomes ' ')."""
    def repl(m):
        t = m.group()
        if t[0] == "/":
            return " " * len(t)
        if t[0] == "'" or t[:2] == "b'":
            return "' '" if any(b in t for b in "{}()[]") else t
        return '""'
    return common._TOK.sub(repl, common._unnest(text) if common._NESTED.search(text) else text)


def decls(code, name):
    """-> [attribute text] of each top-level `mod <name>;` in `code` (one with a path attribute names another file)."""
    out = []
    for m in re.finditer(r"((?:#\s*\[[^\]]*\]\s*)*)(?:pub(?:\s*\([^)]*\))?\s+)?\bmod\s+%s\s*;" % re.escape(name), code):
        if code.count("{", 0, m.start()) == code.count("}", 0, m.start()) and not re.search(r"\bpath\s*=", m.group(1)):
            out.append(m.group(1))
    return out


def parent(p, files):
    """-> (module name, [the files of p's parent module that exist]) for a module file p, or None for a crate root."""
    d, f = p.rsplit("/", 1)
    name = f[:-3]
    if name in ("lib", "main", "build"):
        return None
    if name == "mod":
        if "/" not in d:
            return None
        d, name = d.rsplit("/", 1)
    cands = [d + ".rs", d + "/mod.rs"] + ([d + "/lib.rs", d + "/main.rs"] if d.endswith("/src") else [])
    return name, [c for c in cands if c in files]


def proof(p, files, memo):
    """-> the declaring file when p is a unit-test module (module doc), else None."""
    if p in memo:
        return memo[p]
    memo[p] = None
    up = parent(p, files)
    if up:
        name, cands = up
        found = [(c, a) for c in cands for a in decls(_code(files[c].decode("utf-8", "replace")), name)]
        if found and len({c for c, _ in found}) == 1:
            c = found[0][0]
            if all(_CFG.search(a) for _, a in found):
                memo[p] = c
            elif proof(c, files, memo):
                memo[p] = c + " (itself test-only)"
    return memo[p]


def seed(root, write=False, out=sys.stdout):
    rt._FILES.clear()
    files = rt.scope_files(root)
    found = rt.scan(root)
    have = set()
    for d, w, what in ((rt.REGISTRY, 5, "registry"), (rt.DEBT, 5, "TLS-DEBT"), (rt.SCOPE_DEBT, 4, "SCOPE-DEBT")):
        have |= set(rt.parse_rows(rt.read_dir(root, d), w, what)[0])
    reg, debt, memo = collections.defaultdict(list), collections.defaultdict(list), {}
    for p, bs in sorted(found.items()):
        for s in dict.fromkeys(s for b in bs for s in b):
            if (p, s) in have:
                continue
            why = proof(p, files, memo)
            if why:
                reg[p].append(f"{p}\t{s}\t{OWNER}\ttest\ttest\t#[cfg(test)] mod of {why}\n")
            else:
                debt[p].append(f"{p}\t{s}\t{LANE}\t{MISSING}\n")
    n_reg, n_debt = sum(map(len, reg.values())), sum(map(len, debt.values()))
    print(f"unclassed statics: {n_reg + n_debt}; unit-test registry rows {n_reg} in {len(reg)} files; SCOPE-DEBT rows "
          f"{n_debt} in {len(debt)} files", file=out)
    for p in sorted(p for p in debt if re.search(r"(^|/|_)tests?(_|\.rs$)", p.rsplit("/", 1)[1])):
        print(f"  not proven a unit-test module: crates/{p} ({len(debt[p])} statics)", file=out)
    if write:
        for d, rows in ((rt.REGISTRY, reg), (rt.SCOPE_DEBT, debt)):
            os.makedirs(os.path.join(root, d), exist_ok=True)
            for p, lines in rows.items():
                with open(os.path.join(root, d, rt.file_name(p)), "a", encoding="utf-8") as fh:
                    fh.write("".join(lines))
    return reg, debt


def selftest():
    """The proof on planted trees: what proves a unit-test module and what does not."""
    cases = [  # (files, the module file, proven)
        ({"lib.rs": "#[cfg(test)]\nmod tests;\n", "tests.rs": ""}, "tests.rs", True),
        ({"lib.rs": "pub mod a;\n", "a.rs": "#[cfg(test)]\n#[allow(dead_code)]\npub(crate) mod t;\n", "a/t.rs": ""}, "a/t.rs", True),
        ({"lib.rs": "mod a;\n", "a/mod.rs": "#[cfg(test)]\nmod t;\n", "a/t.rs": ""}, "a/t.rs", True),
        ({"lib.rs": "#[cfg(test)]\nmod t;\n", "t/mod.rs": "mod u;\n", "t/u.rs": ""}, "t/u.rs", True),         # its parent is
        ({"lib.rs": "#[cfg(any(test, feature = \"fuzz\"))]\npub mod tests_dump;\n", "tests_dump.rs": ""}, "tests_dump.rs", False),
        ({"lib.rs": "// #[cfg(test)]\nmod tests;\n", "tests.rs": ""}, "tests.rs", False),                       # a comment
        ({"lib.rs": "mod inner {\n    #[cfg(test)]\n    mod tests;\n}\n", "tests.rs": ""}, "tests.rs", False),   # not top level
        ({"lib.rs": "#[cfg(test)]\nmod tests;\n#[cfg(feature = \"x\")]\nmod tests;\n", "tests.rs": ""}, "tests.rs", False),
        ({"lib.rs": "#[cfg(test)]\n#[path = \"other.rs\"]\nmod tests;\n", "tests.rs": ""}, "tests.rs", False),  # #[path]
    ]
    bad = 0
    for k, (fs, p, want) in enumerate(cases):
        files = {"x/src/" + f: t.encode() for f, t in fs.items()}
        got = bool(proof("x/src/" + p, files, {}))
        print(f"  case {k}: {'proven' if got else 'debt'}" + ("" if got == want else "  FAIL"))
        bad += got != want
    tmp = tempfile.mkdtemp(prefix="rt-seed-")
    try:
        for f, t in (("crates/y/src/lib.rs", "#[cfg(test)]\nmod tests;\nthread_local! { static P: u8 = 0; }\n"),
                     ("crates/y/src/tests.rs", "thread_local! { static T: u8 = 0; }\n")):
            os.makedirs(os.path.dirname(os.path.join(tmp, f)), exist_ok=True)
            with open(os.path.join(tmp, f), "w") as fh:
                fh.write(t)
        reg, debt = seed(tmp, write=True, out=open(os.devnull, "w"))
        rt._FILES.clear()
        again = seed(tmp, out=open(os.devnull, "w"))
        ok = list(reg) == ["y/src/tests.rs"] and list(debt) == ["y/src/lib.rs"] and again == ({}, {})
        print(f"  write + rerun: {'ok' if ok else 'FAIL'}")
        bad += not ok
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    print(f"selftest: {'RED' if bad else 'GREEN'}")
    return 1 if bad else 0


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--root", default=os.path.abspath(os.path.join(HERE, "..", "..", "..", "..")))
    ap.add_argument("--write", action="store_true")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    sys.exit(selftest() if a.selftest else (seed(a.root, a.write), 0)[1])
