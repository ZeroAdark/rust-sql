#!/usr/bin/env python3
"""The compiler half of R6 (PLAN-v5-constructs #10): the unsafe_code attributes the serial step adds.

    attrs.py --plan [--root DIR]     print what would be added
    attrs.py --apply [--root DIR]    add it (idempotent); then `cargo check --all-targets` on a box decides

A crate whose src/ has no unsafe construct gets `#![forbid(unsafe_code)]` at its root. Every other crate root gets
`#![deny(unsafe_code)]`, and each module file that holds an unsafe construct gets `#![allow(unsafe_code)]` as its first
inner attribute. R6 then keeps `allow(unsafe_code)` inside raw.rs, mem, contract, the substrate and LEGACY, so the
compiler refuses new unsafe in every other file. Where the compiler still reports unsafe_code (code a macro defined in
another file expands to), add the allow to the file it names."""
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from common import G, SUBSTRATE, Src, crate_dir, is_crate_root, read, walk_sources, word_start  # noqa: E402
import r06_unsafe  # noqa: E402

DENY, FORBID, ALLOW = "#![deny(unsafe_code)]", "#![forbid(unsafe_code)]", "#![allow(unsafe_code)]"
_ATTR = re.compile(r"#!\[\s*(?:deny|forbid|allow)\(\s*unsafe_code\s*\)\s*\]")


def plan(root):
    """-> [(path, attribute or a note)]: substrate crates get nothing (their own modules are R6's home; the text rule
    covers their ops/); a crate with no unsafe gets forbid; a crate whose root holds no unsafe gets deny at the root and
    allow in each module file holding unsafe; a crate whose root holds unsafe gets nothing yet (R6 keeps its root
    directory on LEGACY until that unsafe moves out, and then the deny goes in)."""
    by_crate, roots = {}, {}
    for p in walk_sources(root):
        if not p.startswith(G):
            continue
        c = crate_dir(p)
        src = Src(p, read(root, p))
        unsafe = bool(src.hits(r06_unsafe.PATTERN, keep=word_start)) and any(
            "unsafe_code" not in src.code[n - 1] for n in src.hits(r06_unsafe.PATTERN, keep=word_start))
        by_crate.setdefault(c, []).append((p, unsafe))
        if is_crate_root(p):
            roots[c] = p
    out = []
    for c in sorted(by_crate):
        if c not in roots:
            continue
        if c in SUBSTRATE:
            out.append((roots[c], "SUBSTRATE (no attribute; R6's home outside ops/)"))
            continue
        files = by_crate[c]
        if not any(u for _, u in files):
            out.append((roots[c], FORBID))
        elif dict(files)[roots[c]]:
            out.append((roots[c], "ROOT-HOLDS-UNSAFE (deny goes in when the root's unsafe moves out; LEGACY until then)"))
        else:
            out.append((roots[c], DENY))
            out += [(p, ALLOW) for p, u in files if u]
    return out


def insert(text, attr):
    if _ATTR.search(text.split("\n\n", 1)[0] + "\n" + "\n".join(text.split("\n")[:40])):
        return text
    lines = text.split("\n")
    i = 0
    while i < len(lines):
        s = lines[i].strip()
        if s == "" or s.startswith("//") or (s.startswith("#![") and s.endswith("]")):
            i += 1
            continue
        break
    while i > 0 and lines[i - 1].strip() == "":
        i -= 1
    return "\n".join(lines[:i] + [attr] + lines[i:])


def main():
    root = os.path.abspath(os.path.join(HERE, "../../../.."))
    if "--root" in sys.argv:
        root = sys.argv[sys.argv.index("--root") + 1]
    items = plan(root)
    if "--apply" in sys.argv:
        for p, a in items:
            if a.startswith("#!"):
                fp = os.path.join(root, p)
                t = read(root, p)
                n = insert(t, a)
                if n != t:
                    with open(fp, "w") as fh:
                        fh.write(n)
    for p, a in items:
        print(f"{a}\t{p}")


if __name__ == "__main__":
    main()
