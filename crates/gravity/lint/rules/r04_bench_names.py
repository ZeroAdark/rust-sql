"""R4: a benchmark name in production source (PLAN-v5 §3.2; constructs #18). Benchmark names belong in tests."""
import re
from common import G, in_planner, in_src, word_start

ID = "R4"
TITLE = "a benchmark name (ClickBench / TPC-H query, census statement) in production source"
INSTEAD = "describe the form the code handles, here and in its tests"
HOME = []


def SCOPE(p):
    return in_src(p) or in_planner(p)


_COMMENT = re.compile(r"(?:[qQ]\d{1,2}|TH\d\d|ClickBench|clickbench|TPC-H|tpch|TPCH|bench-\d{3})\b"
                      r"|(?:cb|cb2m|th|tpch)_[a-z]+-q?\d{2,4}\b")
_CODE = re.compile(r"[cC][lL][iI][cC][kK][bB][eE][nN][cC][hH]|[tT][pP][cC][hH]|TH\d\d\b|Q\d{1,2}\b")


def check(src, ctx):
    return sorted(set(src.hits(_COMMENT, "comments", keep=word_start) + src.hits(_CODE, keep=_word_or_name)))


def _word_or_name(t, m):
    return m.group()[0] in "cCtT" and m.group()[1] != "H" or word_start(t, m)


PLANTED = [(G + "lowerv4/src/ops/sort/planted.rs", "// the q23 fast path: ClickBench's URL top-k\npub fn f() {}\n")]
CLEAN = [(G + "lowerv4/src/ops/sort/clean.rs", "// a top-k over a text key\npub fn f(q: u32) -> u32 { q }\n")]
