"""R7: `transmute` and `Mcx<'static>` (lifetime extension) outside mem and contract (PLAN-v5 §3.2; constructs #10, #14)."""
import re
from common import G, home, in_any, in_src, is_substrate

ID = "R7"
TITLE = "transmute or Mcx<'static> outside mem, contract, raw.rs and the substrate"
INSTEAD = "a scoped context from mem (one per re-open), or a typed handle from contract::slots"
SCOPE = in_src
_HOME = home("raw", "mem", "contract")


def is_home(p):
    return in_any(p, _HOME) or is_substrate(p)


PATTERN = re.compile(r"\btransmute(?:_copy)?\b|\bMcx\s*<\s*'static\s*>")
NEEDS = ("transmute", "'static")   # any of these in the raw text, or the file is skipped
PLANTED = [(G + "exec/src/ops/sort/planted.rs", "pub fn keep(m: &Mcx<'_>) -> &Mcx<'static> { todo!() }\n")]
CLEAN = [(G + "exec/src/ops/sort/clean.rs", "pub fn keep<'m>(m: &Mcx<'m>) -> &Mcx<'m> { m }\n")]
