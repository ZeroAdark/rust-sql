"""RSN: a server thread spawned without `pagegive::pinned` (D4689, memacct 2026-10-16). glibc's thread-exit
advise_stack_range gives a dead thread's whole stack reservation back to the kernel unless a page of it is mlocked;
`pagegive::pinned(f)` locks one page (and counts / warns when the lock fails). Every production
`std::thread::spawn`, `thread::Builder::..spawn(..)`, `spawn_scoped` and `pthread_create` under crates/ must carry
`pagegive::pinned` in its statement (the next 8 lines), or an exempt row. No debt list: the tree has none today.

Left out: tests, benches, examples, crates/bin tools and the lint itself; items gated by a `#[cfg(...)]` that names `test`
(`cfg(test)`, `cfg(all(test, ..))`), `#[test]` fn bodies, `scope(..)` spawns (a scoped thread ends before its scope) and
the pgrust_sim world (`sim_world.rs`, `sim/`), where the scheduler owns thread creation."""
import os
import re

from common import G, in_any

ID = "RSN"
TITLE = "a server thread spawned without pagegive::pinned"
INSTEAD = "wrap the thread body in pagegive::pinned(..): `.spawn(pagegive::pinned(move || ..))`"
GLOBAL = True
ROOTS = ("crates/",)
SKIP = ("crates/postgres-18.6-reference/", "crates/context/", "crates/bin/", G + "lint/")
SPAWN = re.compile(r"thread::spawn\s*\(|\bpthread_create\s*\(|\bspawn_scoped\s*\(|\bBuilder::new\s*\(\s*\)")
CFG_TEST = re.compile(r"^\s*#\[\s*cfg\s*\((?=[^\]]*\btest\b)(?![^\]]*\bnot\s*\(\s*test)")
TEST_FN = re.compile(r"^\s*#\[\s*(?:test|tokio::test)\s*\]")
FN = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+\"C\"\s+)?fn\s")
WINDOW = 8


def is_skipped(p):
    base = os.path.basename(p)
    return (in_any(p, list(SKIP)) or "/tests/" in p or "/benches/" in p or "/bench/" in p or "/examples/" in p or "/test/" in p
            or "/sim/" in p or base in ("tests.rs", "test.rs", "sim_world.rs") or base.endswith(("_tests.rs", "_test.rs")))


def sources(ctx):
    out = []
    for r in ROOTS:
        for dp, dn, fn in os.walk(os.path.join(ctx.root, r)):
            dn[:] = sorted(d for d in dn if d not in ("target", "tests", "benches", "examples") and not d.startswith("."))
            for f in sorted(fn):
                if f.endswith(".rs"):
                    p = os.path.relpath(os.path.join(dp, f), ctx.root).replace(os.sep, "/")
                    if not is_skipped(p):
                        out.append(p)
    return out


def blank_items(lines, start_re):
    """a copy of `lines` with each item that follows a line matching `start_re` (its attribute) blanked, by brace matching"""
    out = list(lines)
    i = 0
    while i < len(out):
        if start_re.match(out[i]):
            j, depth, seen = i, 0, False
            while j < len(out):
                if j > i and not seen and FN.match(out[j]) is None and not out[j].lstrip().startswith(("#", "mod ", "pub", "unsafe", "async", "impl", "fn")):
                    pass
                for ch in out[j]:
                    if ch == "{":
                        depth += 1
                        seen = True
                    elif ch == "}":
                        depth -= 1
                if j > i and seen and depth <= 0:
                    break
                if j > i and not seen and out[j].rstrip().endswith(";"):
                    break
                j += 1
            for k in range(i, min(j + 1, len(out))):
                out[k] = ""
            i = j + 1
        else:
            i += 1
    return out


def scan(ctx):
    hits = []
    for p in sources(ctx):
        text = ctx.read(p)
        if text is None or ("spawn" not in text and "pthread_create" not in text):
            continue
        lines = blank_items(blank_items(text.split("\n"), CFG_TEST), TEST_FN)
        for i, l in enumerate(lines):
            t = l.strip()
            if t.startswith("//") or not SPAWN.search(t) or "scope" in t:
                continue
            win = " ".join(lines[i:i + WINDOW])
            if "Builder::new" in t and ".spawn" not in win and "pthread_create" not in win:
                continue
            if "pagegive::pinned" not in win:
                hits.append((p, i + 1, t[:100]))
    return hits


def check_tree(ctx):
    return [(p, n, f"`{t}` spawns a thread without pagegive::pinned; instead: {INSTEAD}") for p, n, t in scan(ctx)]


PLANTED_TOML = ("crates/backend/postmaster/planted/src/lib.rs",
                "pub fn go() {\n    let h = std::thread::Builder::new().name(\"x\".into()).spawn(move || work()).unwrap();\n    h.join().unwrap();\n}\n")
CLEAN_TOML = ("crates/backend/postmaster/clean/src/lib.rs",
              "pub fn go() {\n    let h = std::thread::Builder::new().name(\"x\".into()).spawn(pagegive::pinned(move || work())).unwrap();\n    h.join().unwrap();\n}\n"
              "#[cfg(all(test, not(loom)))]\nmod tests {\n    #[test]\n    fn t() { std::thread::spawn(|| 1).join().unwrap(); }\n}\n"
              "#[cfg(test)]\nfn helper() { std::thread::spawn(|| 2); }\n"
              "#[test]\nfn plain() { std::thread::spawn(|| 3); }\n"
              "pub fn scoped() { std::thread::scope(|s| { s.spawn(|| 4); }); }\n")
MORE_PLANTED_TOML = [
    ("crates/backend/postmaster/planted2/src/lib.rs",       # a bare thread::spawn, and a cfg(not(test)) item is production
     "#[cfg(not(test))]\npub fn go() { std::thread::spawn(|| 1); }\n"),
]
