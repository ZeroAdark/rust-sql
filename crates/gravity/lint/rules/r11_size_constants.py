"""R11: a size or threshold constant outside cost and contract::layout (PLAN-v5 §3.2; constructs #19). Only named
`const` / `static` items with a numeric value and a threshold word in the name; comparisons with literals are not
linted (1,315 of them, RO M3) and are left to the end audit."""
import re
from common import G, home, in_any, in_src

ID = "R11"
TITLE = "a numeric size / threshold constant outside cost and contract::layout"
INSTEAD = "gravity_program::cost (derived from statistics, citing its source) or contract::layout"
SCOPE = in_src
_HOME = home("cost", "layout")


def is_home(p):
    return in_any(p, _HOME)


# opcode names (PF_MIN_I64, OP_MAX ...) name an operation, not a bound: excluded
PATTERN = re.compile(
    r"\b(?:const|static)\s+(?!(?:PF|OP)_)(?:[A-Z0-9_]*_)?(?:MAX|MIN|CAP|LIMIT|THRESH[A-Z]*|FLOOR|CEIL[A-Z]*|CUTOFF|BUDGET|TARGET"
    r"|RATIO|FACTOR|FRACTION|CHUNK|BATCH|MORSEL|FANOUT|GROW[A-Z]*|SMALL|LARGE|BIG)(?:_[A-Z0-9_]*)?\s*:\s*"
    r"(?:u8|u16|u32|u64|u128|usize|i8|i16|i32|i64|isize|f32|f64)\s*=\s*[0-9][0-9_.]*(?:\s*(?:<<|\*)\s*[0-9_]+)*\s*;")
# A member of a code or index enumeration names a params word or a kind, not a bound (P_LIMIT, the word that holds the
# LIMIT; S_MIN_CODE, a min(text) slot; OUT_MIN, a min output): its prefix, the name before the first `_` and not itself
# a threshold word, is shared by at least 3 integer-literal consts of the file whose values are exactly 0 .. n-1. A bound
# family (T1_MIN_SLOTS = 1024, T1_MAX_PARTS = 2048) is never such a run, and MAX_KEYS-style names are never exempt.
_DECL = re.compile(r"\b(?:const|static)\s+([A-Z][A-Z0-9]*)_[A-Z0-9_]+\s*:\s*(?:u8|u16|u32|u64|u128|usize|i8|i16|i32|i64|isize)"
                   r"\s*=\s*([0-9][0-9_]*)\s*;")
_WORD = re.compile(r"(?:MAX|MIN|CAP|LIMIT|THRESH[A-Z]*|FLOOR|CEIL[A-Z]*|CUTOFF|BUDGET|TARGET|RATIO|FACTOR|FRACTION|CHUNK|BATCH"
                   r"|MORSEL|FANOUT|GROW[A-Z]*|SMALL|LARGE|BIG)$")


def _enumerations(text):
    fams = {}
    for m in _DECL.finditer(text):
        fams.setdefault(m.group(1), []).append(int(m.group(2).replace("_", "")))
    return {p for p, vs in fams.items() if len(vs) >= 3 and sorted(vs) == list(range(len(vs))) and not _WORD.match(p)}


def check(src, ctx):
    hits = src.hits(PATTERN)
    enums = _enumerations(src.joined("code")) if hits else set()
    return [n for n in hits if not ((m := _DECL.search(src.code[n - 1])) and m.group(1) in enums)]


NEEDS = ("const", "static")   # any of these in the raw text, or the file is skipped
PLANTED = [(G + "hosts/src/ops/agg/planted.rs", "const TWO_PASS_MIN_GROUPS: u64 = 1 << 16;\n"),
           # a bound family is not an enumeration (its values are not 0 .. n-1)
           (G + "hosts/src/ops/agg/planted_family.rs", "const T1_NONE: u32 = 0;\nconst T1_MIN_SLOTS: u64 = 1024;\nconst T1_MAX_PARTS: u32 = 2048;\n"),
           # a threshold-word prefix is never an enumeration
           (G + "hosts/src/ops/agg/planted_prefix.rs", "const MAX_A: usize = 0;\nconst MAX_B: usize = 1;\nconst MAX_C: usize = 2;\n"),
           # two members are not an enumeration
           (G + "hosts/src/ops/agg/planted_pair.rs", "const P_SHAPE: usize = 0;\nconst P_LIMIT: usize = 1;\n")]
CLEAN = [(G + "hosts/src/ops/agg/clean.rs", "const W_STATE: usize = 3;\nconst NONE: u32 = u32::MAX;\n"),
         # params word indexes and kind codes: members of 0 .. n-1 enumerations
         (G + "hosts/src/ops/agg/clean_codes.rs", "const P_LIMIT: usize = 0;\nconst P_OFFSET: usize = 1;\nconst P_SHAPE: usize = 2;\n"
                                                  "pub const S_COUNT: u8 = 0;\npub const S_SUM: u8 = 1;\npub const S_MIN_CODE: u8 = 2;\n")]
