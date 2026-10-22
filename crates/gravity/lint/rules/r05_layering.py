"""R5: a crate layering edge outside today's graph (PLAN-v5 §3.2; constructs #27). The ceiling is the gravity_* edge set
of every Cargo.toml under crates/gravity at 2bc7250b217 ([dependencies], [dev-dependencies], [build-dependencies], target
tables), with lowerv4 read as lower and entry as exec, so the planned merges add no edge. Plus the one edge the plan adds
(decompose -> fn, which exists transitively). On a landing (--base) edges are also a ratchet: no edge the parent lacks.
The lower <-> exec dev-dependency cycle is in the ceiling until a landing removes it; the runner prints it each run.
A test-only package (its owner's integration tests or test rig, never linked into the server) is read as its owner, so
its edges must already be the owner's; the owner <-> its test package edge is then a self-edge, listed in ADDED."""
import os
import re

from common import G

ID = "R5"
TITLE = "a crate dependency edge outside the frozen layering"
INSTEAD = "reach the code through a crate you already depend on, or move it down a layer"
GLOBAL = True
ALIAS = {"lowerv4": "lower", "entry": "exec",
         "lowerv4_it": "lower", "hosts_it": "hosts", "hosts_rig": "hosts",
         "driver_rig": "driver", "kernels_rig": "kernels"}   # test-only packages (BD-3/11/12)
CYCLE = {("lower", "exec")}            # dev-only; goes when lower's tests that need exec move to exec/tests
ADDED = {("decompose", "fn"),           # PLAN-v5-constructs #27: already reached through program -> kernels -> fn
         ("backend", "wasm"),           # lane wasmjit stage 2 (main GO 2026-10-07): wasm-target only, the third sibling backend
         ("exec", "deps"), ("hosts", "deps"), ("backend", "deps"), ("fn", "deps"),   # C3 recorded dependencies (main GO 2026-10-11T06:14:21Z): the leaf recorder
         ("lower", "deps"),             # C3 D3938 (main 2026-10-11T08:56:45Z): lowerv4's condition cache key reads the covered quals' recorded deps
         ("hosts", "hosts"), ("driver", "driver"), ("kernels", "kernels")}   # owner <-> its test-only package (ALIAS; BD-11, BD-12)
CEILING = {
    "abi": set(), "asm": {"abi", "kernels"},
    "backend": {"abi", "driver", "interp", "ir", "kernels", "parallel", "program", "tpde"},
    "decompose": {"abi", "lint", "program"}, "dict": {"abi"}, "driver": {"abi", "program"},
    "exec": {"abi", "backend", "decompose", "driver", "entry_seams", "exec", "expr", "fn", "hosts", "kernels", "lower",
             "parallel", "program", "wire"},
    "expr": {"abi", "backend", "dict", "fn", "ir", "lint", "program", "wasm"}, "fn": {"abi", "lint"},
    "hosts": {"abi", "decompose", "dict", "driver", "fn", "kernels", "parallel", "program"}, "ir": set(),
    "kernels": {"abi", "dict", "fn", "program"}, "lint": set(),
    "lower": {"abi", "backend", "decompose", "dict", "driver", "exec", "expr", "fn", "interp", "ir",
              "kernels", "lint", "lower", "program", "tpde"},
    "oltp_lab": set(), "parallel": {"abi", "driver", "kernels", "parallel", "program"},
    "pool": {"abi", "driver", "hosts", "kernels", "parallel", "program"}, "prewarm": {"backend", "exec", "hosts"},
    "program": {"abi", "kernels"}, "wire": {"abi", "driver", "program"},
    "interp": None, "tpde": None, "wasm": None,     # backend's nested packages: not checked
}

_DEP = re.compile(r"^\s*(gravity[_-][a-z0-9_]+)\s*(?:\.|=)")
_SECTION_DEP = re.compile(r"^\s*\[(?:.*\.)?(?:dev-|build-)?dependencies\.(gravity[_-][a-z0-9_]+)\]")


def _name(n):
    n = n.replace("-", "_")
    n = n[len("gravity_"):] if n.startswith("gravity_") else n
    return ALIAS.get(n, n)


def manifests(root):
    out = []
    for dp, dn, fn in os.walk(os.path.join(root, G)):
        dn[:] = sorted(d for d in dn if d not in ("target", "src", "tests", "benches"))
        if "Cargo.toml" in fn:
            out.append(os.path.relpath(os.path.join(dp, "Cargo.toml"), root).replace(os.sep, "/"))
    return sorted(out)


def edges(read, paths):
    """-> {(crate, dep): (path, line)} over the manifests `paths`, read through `read(path) -> text or None`."""
    out = {}
    for p in paths:
        text = read(p)
        if text is None:
            continue
        pkg, sec = None, ""
        for n, line in enumerate(text.split("\n"), 1):
            s = line.split("#", 1)[0].strip()
            if s.startswith("["):
                sec = s
                m = _SECTION_DEP.match(s)
                if m and pkg:
                    out.setdefault((pkg, _name(m.group(1))), (p, n))
                continue
            if sec == "[package]" and pkg is None:
                m = re.match(r'name\s*=\s*"([^"]+)"', s)
                if m:
                    pkg = _name(m.group(1))
                continue
            if pkg and "dependencies" in sec and not sec.startswith("[workspace"):
                m = _DEP.match(s)
                if m:
                    out.setdefault((pkg, _name(m.group(1))), (p, n))
    return out


def check_tree(ctx):
    """Edges over the ceiling; on a landing, edges the parent lacks. -> [(path, line, note)]"""
    tip = edges(ctx.read, manifests(ctx.root))
    out = []
    for (a, b), (p, n) in sorted(tip.items()):
        allowed = CEILING.get(a, set())
        if allowed is None:
            continue
        if b not in allowed and (a, b) not in ADDED:
            out.append((p, n, f"{a} -> {b} is outside the frozen layering"))
    if ctx.base_read is not None:
        base = edges(ctx.base_read, ctx.base_manifests)
        for (a, b), (p, n) in sorted(tip.items()):
            if (a, b) not in base and CEILING.get(a) is not None and (a, b) not in ADDED:
                out.append((p, n, f"{a} -> {b} is new on this landing (edges are only removed)"))
    ctx.notes.append("R5: the lower <-> exec dev-dependency cycle is still in the graph" if ("lower", "exec") in tip
                     else "R5: the lower <-> exec dev cycle is gone")
    return out


PLANTED_TOML = ("crates/gravity/kernels/Cargo.toml",
                '[package]\nname = "gravity_kernels"\n[dependencies]\ngravity_abi = { path = "../abi" }\n'
                'gravity_exec = { path = "../exec" }\n')
CLEAN_TOML = ("crates/gravity/kernels/Cargo.toml",
              '[package]\nname = "gravity_kernels"\n[dependencies]\ngravity_abi = { path = "../abi" }\n'
              'gravity_fn.workspace = true\n')
# a test-only package is read as its owner: an edge its owner may not have hits; the owner's own edges pass
MORE_PLANTED_TOML = (("crates/gravity/hosts_it/Cargo.toml",
                      '[package]\nname = "gravity_hosts_it"\n[dev-dependencies]\ngravity_hosts = { path = "../hosts" }\n'
                      'gravity_exec = { path = "../exec" }\n'),)
MORE_CLEAN_TOML = (("crates/gravity/hosts_it/Cargo.toml",
                    '[package]\nname = "gravity_hosts_it"\n[dev-dependencies]\ngravity_hosts = { path = "../hosts" }\n'
                    'gravity_abi = { path = "../abi" }\n'),)
