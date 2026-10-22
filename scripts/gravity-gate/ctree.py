#!/usr/bin/env python3
"""The CRATE tree: what the gate's binary is built from (docs/gravity/gate.md §9). Everything tracked under crates/, plus
Cargo.lock, Cargo.toml, rust-toolchain.toml and .cargo — except that every Cargo.toml is read with the test-only flags
stripped (`doctest` / `test` / `bench` / `doc` under [lib], [[test]], [[bench]], [[example]], [[bin]]) and .cargo/config.toml
with its `[env] KACHE_*` lines stripped (build-cache knobs): a manifest line that changes no binary (the docs batch's
`doctest = false` x4 + KACHE_CACHE_EXECUTABLES cost three lanes a re-gate, 2026-09-21) is not a crate-tree change.

usage: ctree.py hash <repo> <rev>            one 40-hex hash of the crate tree at <rev>
       ctree.py diff <repo> <a> <b>          the crate-tree paths that differ materially between <a> and <b> (empty, rc 0 = the same binary)
"""
import hashlib
import re
import subprocess
import sys

PATHS = ["crates", "Cargo.lock", "Cargo.toml", "rust-toolchain.toml", ".cargo"]
TEST_SECTIONS = ("[lib]", "[[test]]", "[[bench]]", "[[example]]", "[[bin]]")
TEST_KEYS = re.compile(r"^(doctest|test|bench|doc)\s*=")
CACHE_KEYS = re.compile(r"^KACHE_\w*\s*=")


def git(repo, *args, data=None):
    return subprocess.run(["git", "-C", repo, *args], input=data, capture_output=True, check=True).stdout


def filtered(path, text):
    """the manifest with the lines that change no binary removed (None for a path that is hashed as is)."""
    if path.endswith("Cargo.toml"):
        sections, keys = TEST_SECTIONS, TEST_KEYS
    elif path.endswith(".cargo/config.toml") or path == ".cargo/config.toml":
        sections, keys = ("[env]",), CACHE_KEYS
    else:
        return None
    out, sec = [], ""
    for line in text.decode("utf-8", "replace").split("\n"):
        s = line.strip()
        if s.startswith("["):
            sec = s
        elif sec.startswith(sections) and keys.match(s):
            continue
        out.append(line)
    return "\n".join(out).encode()


def blobs(repo, rev):
    """path -> object id for the crate tree at rev."""
    out = {}
    for rec in git(repo, "ls-tree", "-r", "-z", rev, "--", *PATHS).split(b"\0"):
        if rec:
            meta, path = rec.split(b"\t", 1)
            out[path.decode()] = meta.split()[2].decode()
    return out


def cat_all(repo, oids):
    """oid -> content, one `git cat-file --batch` for all of them."""
    oids = sorted(set(o for o in oids if o))
    if not oids:
        return {}
    data = git(repo, "cat-file", "--batch", data=("\n".join(oids) + "\n").encode())
    out, i = {}, 0
    while i < len(data):
        j = data.index(b"\n", i); head = data[i:j].split(); i = j + 1
        if len(head) < 3:
            continue
        n = int(head[2]); out[head[0].decode()] = data[i:i + n]; i += n + 1
    return out


def manifest_key(path, oid, content):
    f = filtered(path, content.get(oid, b""))
    return oid if f is None else "filtered:" + hashlib.sha256(f).hexdigest()


def hash_tree(repo, rev):
    b = blobs(repo, rev)
    content = cat_all(repo, [o for p, o in b.items() if filtered(p, b"") is not None])
    h = hashlib.sha256()
    for path, oid in sorted(b.items()):
        h.update(f"{manifest_key(path, oid, content)} {path}\n".encode())
    return h.hexdigest()[:40]


def diff(repo, a, b):
    ba, bb = blobs(repo, a), blobs(repo, b)
    changed = [p for p in sorted(set(ba) | set(bb)) if ba.get(p) != bb.get(p)]
    content = cat_all(repo, [ba.get(p) for p in changed] + [bb.get(p) for p in changed])
    out = []
    for p in changed:
        if filtered(p, b"") is not None and filtered(p, content.get(ba.get(p), b"")) == filtered(p, content.get(bb.get(p), b"")):
            continue
        out.append(p)
    return out


def main():
    if len(sys.argv) == 4 and sys.argv[1] == "hash":
        print(hash_tree(sys.argv[2], sys.argv[3])); return 0
    if len(sys.argv) == 5 and sys.argv[1] == "diff":
        d = diff(sys.argv[2], sys.argv[3], sys.argv[4])
        for p in d:
            print(p)
        return 0
    print(__doc__); return 2


if __name__ == "__main__":
    sys.exit(main())
