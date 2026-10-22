"""C4: per-type semantics come from typesem (constructs #4; DECOMPOSER-v5 §3 item 5). Outside typesem: no function named
type_of / typmod_of / typmod / collation_of / expr_type / expr_typmod / expr_collation / result_type, and (outside
decompose, the plan reader, and catalog) no typbyval, collation or operator-family lookup."""
import re
from common import G, home, in_any, in_src, word_start

ID = "C4"
TITLE = "a private type-property derivation or a typbyval / collation / opfamily lookup outside typesem"
INSTEAD = "gravity_fn::typesem (equality, order, hash, collation, by-value width, through the operator class)"
SCOPE = in_src
_HOME = home("typesem", "catalog")


def is_home(p):
    return in_any(p, _HOME)


NAMES = re.compile(r"fn\s+(?:type_of|typmod_of|typmod|collation_of|expr_type|expr_typmod|expr_collation|result_type)\b")
LOOKUP = re.compile(
    r"(?:get_typlenbyval|get_typlenbyvalalign|get_typbyval|get_typlen|get_typcollation|type_is_collatable"
    r"|get_op_opfamily_properties|get_opfamily_proc|get_opfamily_member|get_ordering_op_properties"
    r"|get_op_hash_functions|get_opclass_family|get_opclass_input_type|get_opclass_opfamily_and_input_type"
    r"|get_equality_op_for_ordering_op|get_compatible_hash_operators|get_mergejoin_opfamilies|op_hashjoinable"
    r"|op_mergejoinable)\s*\(")


def check(src, ctx):
    decompose = src.path.startswith(G + "decompose/src/")
    return sorted(set(src.hits(NAMES, keep=word_start) + ([] if decompose else src.hits(LOOKUP, keep=word_start))))


NEEDS = ("type_of", "typmod", "collation_of", "expr_type", "expr_coll", "result_type", "get_typ", "get_op", "get_ordering",
         "get_equality", "get_compatible", "get_mergejoin", "op_hash", "op_merge", "type_is_collatable")   # any of these in the raw text, or the file is skipped
PLANTED = [(G + "lowerv4/src/ops/hashjoin/planted.rs",
            "fn type_of(n: Node) -> u32 { 0 }\npub fn w(t: u32) -> bool { lsyscache::get_typlenbyval(t).1 }\n")]
CLEAN = [(G + "lowerv4/src/ops/hashjoin/clean.rs", "pub fn w(t: &TypeSem) -> bool { t.by_value() }\n")]
