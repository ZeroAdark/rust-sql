"""R13: an address compare of a static, a trait object or a function (D2916; D720 before it). A zero-sized static has no
address of its own (the linker may put another static at it: code.front_dense's table sat at the hashed table's in one
link), and a function has none at all (merged with an identical one, or one copy per codegen unit), so `ptr::eq` /
`addr_eq` over a registry mechanism, a fold class or a static, and any `fn_addr_eq`, can read one as another. Compare by
type (a trait method its implementor answers) or by an explicit discriminant (a kind, a name, the value). The rule reads
the call's own arguments: a static or const named there (an upper-case path segment), a `dyn` type, or `fn_addr_eq` at
all. Two variables that hold statics compared by address are review's: the rule cannot see what they hold."""
import re
from common import G, in_src

ID = "R13"
TITLE = "an address compare (ptr::eq / addr_eq / fn_addr_eq) of a static, a trait object or a function"
INSTEAD = ("compare by type (a trait method its implementor answers: TableMech::hashed, FoldClass::class_kind) or an "
           "explicit discriminant (a kind, a name, the value)")
HOME = []
SCOPE = in_src
NEEDS = ("ptr::eq", "addr_eq")
_CALL = re.compile(r"\b(?:ptr::eq|addr_eq|fn_addr_eq)\s*\(")
_STATIC = re.compile(r"(?<![A-Za-z0-9_])[A-Z][A-Z0-9_]*[A-Z_][A-Z0-9_]*(?![A-Za-z0-9_])")  # FOO, ROW_IMAGE, T_X; not T, T1
_DYN = re.compile(r"\bdyn\b")


def check(src, ctx):
    t = src.joined()
    out = []
    for m in _CALL.finditer(t):
        i, depth = m.end(), 1
        while i < len(t) and depth:
            c = t[i]
            depth += (c == "(") - (c == ")")
            i += 1
        args = t[m.end():i - 1]
        if m.group().startswith("fn_addr_eq") or _DYN.search(args) or _STATIC.search(args):
            out.append(t.count("\n", 0, m.start()) + 1)
    return out


PLANTED = [
    (G + "lowerv4/src/ops/agg/planted.rs",
     "pub fn bare(t: &'static dyn TableMech) -> bool {\n    core::ptr::addr_eq(t, &crate::table::bypass::HASHED as &dyn TableMech)\n}\n"),
    (G + "lowerv4/src/ops/agg/numeric/planted.rs",
     "fn own(a: &Agg, k: KindId) -> bool {\n    core::ptr::addr_eq(a.class as *const dyn FoldClass,\n        class(k) as *const dyn FoldClass)\n}\n"),
    (G + "expr/src/nodes/planted.rs", "pub fn image(shim: &Shim) -> bool {\n    core::ptr::eq(shim, &ROW_IMAGE)\n}\n"),
    (G + "lowerv4/src/frame/planted.rs",
     "fn run(e: Entry) -> bool {\n    core::ptr::fn_addr_eq(e, m8_init_run as Entry)\n}\n"),
]
CLEAN = [
    (G + "lowerv4/src/rerun/clean.rs",
     "fn at(ps: &[PipelineNode<'_>], p: &PipelineNode<'_>) -> Option<usize> {\n    (0..ps.len()).find(|&j| core::ptr::addr_eq(&ps[j], p))\n}\n"),
    (G + "exec/src/clean.rs", "fn same(a: &PlannedStmt<'_>, b: &PlannedStmt<'_>) -> bool {\n    core::ptr::eq(a, b) && a.flag == T1\n}\n"),
    (G + "fn/src/clean.rs", "pub fn eq(a: &MacAddr, b: &MacAddr) -> bool {\n    adt_mac::macaddr_eq(a, b)\n}\n"),
    (G + "lowerv4/src/ops/agg/clean.rs", "fn bare(t: &'static dyn TableMech) -> bool {\n    t.hashed()\n}\n"),
]
