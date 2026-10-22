"""R10: a guard constant named for a bug (PLAN-v5 §3.2; constructs #22). A known-broken form is a `Bug(D-id)` gap in
its family's admit.rs (`Gap::Bug(D(2194), WHY)`, gravity_lower::admit), deleted with the verified root fix and a
witness; never a constant. With --open-bugs FILE (one D-id per line or a BUGS-OPEN.tsv), a `Bug(D-id)` naming a bug that
is not open also fails, LEGACY or not."""
import re
from common import G, in_src

ID = "R10"
TITLE = "a constant named for a bug (a guard), or a Bug(D-id) gap whose bug is closed"
INSTEAD = "a Gap::Bug(D(n), why) gap in ops/<family>/admit.rs, deleted with the fix"
HOME = []
SCOPE = in_src
_CONST = re.compile(r"\b(?:const|static)\s+(?:mut\s+)?([A-Z][A-Z0-9_]*)\s*:")
_DID = re.compile(r"(?:^|_)D\d{3,4}(?:_|$)")
_BUG = re.compile(r"\bBug\s*\(\s*D\s*\(?\s*(\d{3,4})\b")   # Bug(D2194) and the face's Bug(D(2194), why)


_CONST_D = re.compile(r"\b(?:const|static)\s+(?:mut\s+)?[A-Z0-9_]*?(?<![A-Z0-9])D\d{3,4}(?:_[A-Z0-9_]*)?\s*:")


def check(src, ctx):
    return src.hits(_CONST_D)


def check_status(src, ctx):
    """Bug(D-id) gaps whose bug is closed: not LEGACY-able (the gap goes with the fix)."""
    if ctx.open_bugs is None:
        return []
    return [(i + 1, f"Bug(D{m.group(1)}) names a bug that is not open") for i, c in enumerate(src.code)
            for m in _BUG.finditer(c) if "D" + m.group(1) not in ctx.open_bugs]


NEEDS = ("const", "static")   # any of these in the raw text, or the file is skipped
PLANTED = [(G + "lowerv4/src/ops/memoize/planted.rs", "pub const NW_GUARD_D2178: &str = \"D2178\";\n")]
PLANTED_STATUS = [(G + "lowerv4/src/ops/memoize/admit.rs", "pub fn gap() -> Gap { Gap::Bug(D9999) }\n"),
                  (G + "lower/src/ops/sort/admit.rs", "pub fn gap() -> Gap { Gap::Bug(D(9999), \"a shape\") }\n")]
CLEAN_STATUS = [(G + "lowerv4/src/ops/memoize/admit.rs", "pub fn gap() -> Gap { Gap::Bug(D2178) }\n"),
                (G + "lower/src/ops/sort/admit.rs", "pub fn gap() -> Gap { Gap::Bug(D(2178), \"a shape\") }\n")]
STATUS_OPEN = {"D2178"}           # the selftest's open-bug list
CLEAN = [(G + "lowerv4/src/ops/memoize/admit.rs", "pub const PF_ADD128: u32 = 4;\npub fn gap() -> Gap { Gap::Bug(D2178) }\n")]
