"""R9: hand-edited module lists (PLAN-v5 §3.2; constructs #24). Module lines live in each directory's own mod.rs: no
`#[path]` mounts, no generated file lists, and a crate root declares only its direct children (no inline `mod a { mod
b; }` blocks that declare files)."""
import re
from common import G, in_src

ID = "R9"
TITLE = "a #[path] mount, a generated module list, or a nested module list in a crate root"
INSTEAD = "a `mod x;` line in the directory's own mod.rs"
HOME = []
SCOPE = in_src
_PATH = re.compile(r"#\[\s*path\s*=")
_GEN = re.compile(r'include!\s*\(\s*concat!\s*\(\s*env!\s*\(\s*""')
_OPEN = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{")
_DECL = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*;")


def check(src, ctx):
    out = [i + 1 for i, c in enumerate(src.code) if _PATH.search(c) or _GEN.search(c)]
    if re.fullmatch(re.escape(G) + r"[^/]+/src/(?:lib|main)\.rs", src.path):
        depth = 0
        for i, c in enumerate(src.code):
            if depth > 0 and _DECL.match(c):
                out.append(i + 1)
            if _OPEN.match(c) or depth > 0:
                depth += c.count("{") - c.count("}")
    return sorted(set(out))


PLANTED = [(G + "lowerv4/src/lib.rs", "pub mod ops;\npub mod join {\n    pub mod drain;\n}\n"),
           (G + "lowerv4/src/ops/hashjoin/mod.rs", '#[path = "../../join/drain.rs"]\nmod drain;\n')]
CLEAN = [(G + "lowerv4/src/lib.rs", "pub mod ops;\nmod tests_support {\n    pub fn f() {}\n}\n"),
         (G + "lowerv4/src/ops/hashjoin/mod.rs", "mod drain;\npub mod strategies;\n")]
