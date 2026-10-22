"""C6: the numeric contract (dscale, overflow, NaN) lives in typesem::numeric (constructs #6): no dscale arithmetic
outside it."""
import re
from common import G, home, in_any, in_src

ID = "C6"
TITLE = "dscale arithmetic outside typesem::numeric"
INSTEAD = "gravity_fn::typesem::numeric"
SCOPE = in_src
_HOME = home("numeric")


def is_home(p):
    return in_any(p, _HOME)


PATTERN = re.compile(r"\b\w*dscale\w*\s*(?:[-+*/]=?|<<|>>)\s*[\w(]|[\w)]\s*(?:[-+*/]|<<|>>)\s*[\w.]*\b\w*dscale\b"
                     r"|\b(?:max|min)\s*\([^)]*\bdscale|\b\w*dscale\w*\s*\.\s*(?:max|min|saturating_\w+|checked_\w+)\(")
NEEDS = ("dscale",)   # any of these in the raw text, or the file is skipped
PLANTED = [(G + "lowerv4/src/ops/agg/planted.rs", "pub fn s(a: Num, b: Num) -> i16 { a.dscale + b.dscale }\n")]
CLEAN = [(G + "lowerv4/src/ops/agg/clean.rs", "pub fn s(a: Num, b: Num) -> i16 { numeric::add_scale(a, b) }\n")]
