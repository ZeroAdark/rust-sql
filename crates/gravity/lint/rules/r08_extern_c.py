"""R8: an `extern "C"` function or `catch_unwind` outside contract (PLAN-v5 §3.2; constructs #11): JIT-to-host entries
are typed, behind one C-unwind trampoline per thread kind, and a panic takes PG's crash path."""
import re
from common import G, home, in_any, in_src

ID = "R8"
TITLE = 'an extern "C" fn (entry, host function or fn-pointer type) or catch_unwind outside contract'
INSTEAD = "contract::host's entryN / leafN wrappers (kernels/src/contract.rs)"
SCOPE = in_src
_HOME = home("contract", "layout")     # contract::host's entries; contract::layout's fn-pointer types (abi)


def is_home(p):
    return in_any(p, _HOME)


_CODE = re.compile(r'\bextern\s+""\s*fn\b|\bcatch_unwind\b')       # string bodies are blanked in code lines
_RAW = re.compile(r'\bextern\s+"C(?:-unwind)?"\s+fn\b|\bcatch_unwind\b')


def check(src, ctx):
    return [n for n in src.hits(_CODE) if _RAW.search(src.lines[n - 1])]
NEEDS = ("extern", "catch_unwind")   # any of these in the raw text, or the file is skipped
PLANTED = [(G + "kernels/src/ops/agg/planted.rs", 'pub extern "C" fn agg_step(s: u64) -> u64 { s }\n')]
CLEAN = [(G + "kernels/src/ops/agg/clean.rs", "pub fn agg_step(s: u64) -> u64 { s }\n")]
