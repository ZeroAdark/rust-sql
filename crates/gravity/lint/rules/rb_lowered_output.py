"""Rule (b) of constructs #31: the raw accessors that read a node's lowered output appear only in the interface home.
A family reaches its own output through the interface's own-node calls, and another node's only through its answer.
Four of the eight critical live bugs read another operator's internals this way (RW F1)."""
import re
from common import home, in_src, is_lowering

ID = "RB"
TITLE = "a raw read of a node's lowered output (need_kind, order_read, group_sort, ends_in, breakers::) outside frame::iface"
INSTEAD = "frame::iface (the request and the answer; Px::reader_of; iface::kind_impl for a kind's registered implementation, iface::breaker_of / end_breaker / ends_in_sink for a pipeline's end)"
SCOPE = lambda p: in_src(p) and is_lowering(p)
HOME = home("iface")
# a call through the interface home itself (`frame::iface::order_read(..)`) is the INSTEAD form, not a raw read;
# `spill::breakers::` is hashjoin's kind file for the spill.breakers kind, not the breakers accessor
PATTERN = re.compile(r"(?<!spill::)\bbreakers::|(?<!iface::)\b(?:need_kind|order_read|group_sort|ends_in)\b")
NEEDS = ("breakers::", "need_kind", "order_read", "group_sort", "ends_in")   # any of these in the raw text, or the file is skipped
PLANTED = [("crates/gravity/lowerv4/src/ops/mergejoin/planted.rs",
            "pub fn r(cx: &Px, k: KindId) -> bool { need_kind::<SlotRunImpl>(cx, k).reader.is_some() }\n"),
           ("crates/gravity/lowerv4/src/ops/mergejoin/planted_breakers.rs",
            "pub fn r(gx: &Gx, k: usize) -> bool { crate::frame::breakers::of(gx, k).is_some() }\n")]
CLEAN = [("crates/gravity/lowerv4/src/ops/mergejoin/clean.rs",
          "pub fn r(px: &Px, k: usize) -> bool { px.answer(k).order.is_some() }\n"),
         ("crates/gravity/lowerv4/src/ops/agg/clean_iface.rs",
          "pub fn r(gx: &Gx, g: i32) -> bool { crate::frame::iface::order_read(gx, g) }\n"),
         ("crates/gravity/lowerv4/src/ops/clean_spill.rs",
          "pub static S: &[KindId] = &[crate::ops::hashjoin::spill::breakers::ROWS.kinds[0].1];\n")]
