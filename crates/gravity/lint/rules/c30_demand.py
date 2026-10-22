"""C30: a consumer's bound reaches a producer only through the request (Demand; constructs #30). Lowering reads the
Limit op's bound only in the limit family and the interface home."""
import re
from common import L, home, in_any, in_src, is_lowering

ID = "C30"
TITLE = "lowering reads the Limit op's bound outside ops/limit and frame::iface"
INSTEAD = "the Request's bound (frame::iface)"
SCOPE = lambda p: in_src(p) and is_lowering(p)
# effects/visible.rs: `order_visible` names a LIMIT / OFFSET as a reason the row order is visible (D1804), not its bound;
# effects/visible/: its per-family sibling files (main 2026-10-05T00:00:32Z), by the same reason
_HOME = home("iface") + L("ops/limit/") + L("effects/visible.rs") + L("effects/visible/")


def is_home(p):
    return in_any(p, _HOME)


PATTERN = re.compile(r"\bOp::Limit\b|\blimitCount\b|\blimitOffset\b|\blimitOption\b")
NEEDS = ("Op::Limit", "limitCount", "limitOffset", "limitOption")   # any of these in the raw text, or the file is skipped
PLANTED = [("crates/gravity/lowerv4/src/ops/sort/planted.rs",
            "pub fn k(op: &Op) -> Option<u64> { match op { Op::Limit { count, .. } => count.value(), _ => None } }\n")]
CLEAN = [("crates/gravity/lowerv4/src/ops/sort/clean.rs", "pub fn k(r: &Request) -> Option<u64> { r.bound.rows() }\n")]
