"""C29: one definition of each fact (constructs #29; CHANGE-SPECS §6.2: catalog types belong to catalog or typesem). No
`*Facts` / `*Props` type outside facts, catalog, typesem and effects (FuncProps)."""
import re
from common import G, home, in_any, in_src

ID = "C29"
TITLE = "a *Facts / *Props type defined outside facts, catalog, typesem and effects"
INSTEAD = "gravity_decompose::facts::NodeFacts (plan facts), catalog / typesem (catalog rows)"
SCOPE = in_src
# expr and hosts cannot reach decompose: their catalog readers (expr's pg_proc / pg_type rows, hosts' relcache) are
# their crates' catalog modules (DECOMPOSER-v5 §3 item 4)
_HOME = home("facts", "catalog", "typesem", "effects") + [G + "expr/src/catalog.rs", G + "hosts/src/facts.rs"]


def is_home(p):
    return in_any(p, _HOME)


PATTERN = re.compile(r"\b(?:struct|enum|type|trait|union)\s+\w*(?:Facts|Props)\b")
# a type that is not facts in this sense (a marker, a wire record, a test double) is a row of exempt.py
NEEDS = ("Facts", "Props")   # any of these in the raw text, or the file is skipped
PLANTED = [("crates/gravity/lowerv4/src/ops/sort/planted.rs", "pub struct SortFacts { pub ordered: bool }\n"),
           (G + "decompose/src/corpus.rs", "pub struct OtherFacts { pub rels: Vec<u32> }\n")]       # the exemption is per symbol
CLEAN = [("crates/gravity/lowerv4/src/ops/sort/clean.rs", "pub fn f(x: &NodeFacts) -> bool { x.order.is_some() }\n"),
         (G + "decompose/src/corpus.rs", "pub struct FixtureFacts { pub rels: Vec<u32> }\n")]     # an exempted file:symbol
