"""R3, widened: literal OID, type and collation lists and spelling tests (PLAN-v5 §3.2; constructs #17; the widened
form is path100/work-optoverfit/r3wide2.py, less its comparisons of two fields or with 0, which test no spelling)."""
import re
from common import G, home, in_planner, in_src

ID = "R3"
TITLE = "a literal OID / type / collation gate or a spelling test"
INSTEAD = "a catalog name and a typesem / normalize call (a canonical form), never a list"
HOME = home("catalog", "typesem", "normalize")


def SCOPE(p):
    return in_src(p) or in_planner(p)


_F = r"(?:funcid|fnoid|aggfnoid|opno|opfuncid|foid|fn_oid|funcoid)"
_N = (r"(?:funcid|fnoid|aggfnoid|opno|opfuncid|foid|fn_oid|funcoid|vartype|consttype|casetype|typid|typ|ty|type_oid"
      r"|typeoid|coll|collid|inputcollid|key|t|oid|elemtype|restype|resulttype|funcresulttype|aggtype|rettype|atttypid)")
_S = r"(?:funcid|opfuncid|opno|aggfnoid|fnoid|casetype|vartype|consttype|inputcollid)"
_LIT = r"(?:\d{2,5}\b|[A-Z][A-Z0-9_]{2,}\b)"            # an OID literal or a catalog constant (NUMERICOID, F_TEXTEQ)
PATTERN = re.compile("|".join([
    r"\b" + _F + r"\s*(?:==|!=)\s*\d{2,5}\b",                                         # narrow R3
    r"\b" + _F + r"\b[^;\n]{0,60}\b\d{2,5}\s*\|\s*\d{2,5}\b",
    r"matches!\(\s*[\w.()&*]*?\b" + _N + r"\b[\w.()]*\s*,\s*\d{2,5}\s*(?:\||\.\.=)",   # matches!(x, 16 | 700 ..)
    r"const\s+\w+\s*:\s*(?:&\[u32\]|\[u32;\s*\d+\])\s*=\s*&?\[\s*\d{2,5}\s*,",           # const L: &[u32] = &[..]
    r"^\s*\d{2,5}\s*\|\s*\d{2,5}(?:\s*\|\s*\d{2,5})*\s*(?:\|\s*)?=>",                    # 2799 | 2801 => ..
    r"\b" + _N + r"\s*(?:==|!=)\s*\d{2,5}\b",                                         # typid == 700
    r"\b" + _S + r"\b\s*(?:==|!=)\s*" + _LIT,                                         # funcid == F_TEXTEQ
    _LIT + r"\s*(?:==|!=)\s*[\w.]*\b" + _S + r"\b",
    r"\.contains\(\s*&\s*[\w.]*\b" + _S + r"\b\s*\)",                                 # LIST.contains(&f.funcid)
    r"matches!\(\s*[\w.]*\b(?:" + _S[3:-1] + r"|typid|coll)\s*,",                     # matches!(typid, INT4OID | ..)
]), re.M)
_ANCHOR = re.compile(r"[=!]=|matches!|const\s|\.contains\(|\d\s*\|\s*\d")


def check(src, ctx):
    """The full pattern only on lines with a comparison, a matches!, a const, a contains or a `n | m` list (it is slow)."""
    return [n for n in src.hits(_ANCHOR) if PATTERN.search(src.code[n - 1])]


PLANTED = [(G + "lowerv4/src/ops/agg/planted.rs",
            "pub fn quiet(f: &Call) -> bool { f.funcid == 2108 || matches!(f.typid, INT4OID | INT8OID) }\n")]
CLEAN = [(G + "lowerv4/src/ops/agg/clean.rs",
          "pub fn same(a: &Op, b: &Op) -> bool { a.opno == b.opno && a.opfuncid != 0 }\n")]
