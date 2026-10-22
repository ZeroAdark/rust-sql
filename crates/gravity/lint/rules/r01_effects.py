"""R1: an effect test outside the effects home (PLAN-v5 §3.2; constructs #1; DECOMPOSER-v5 §3 item 4)."""
import re
from common import G, home, in_src

ID = "R1"
TITLE = "an effect test (volatility, leakproof, strictness, parallel safety, user-code reach) outside effects"
INSTEAD = "gravity_decompose::effects::{of_expr, of_ops, of_plan} and effects::licence"
# expr's catalog.rs and hosts' facts.rs hold the two pg_proc row types and their constructor; their effect fields go
# crate-private once every reader calls effects (DECOMPOSER-v5 §3 item 4).
HOME = home("effects") + [G + "expr/src/catalog.rs", G + "hosts/src/facts.rs"]
SCOPE = in_src
PATTERN = re.compile(
    r"\bprovolatile\b|\bVolatility::|\bcontain_volatile_functions\b|\bFIRST_NORMAL_OID\b|\bFIRST_NORMAL_OBJECT_ID\b"
    r"|\bFirstNormalObjectId\b|\bleakproof\b|\bproisstrict\b|\bproparallel\b|\bfunc_volatile\s*\("
    r"|\bget_func_leakproof\s*\(|\bfunc_parallel\s*\("
    r"|\bvolatility\b.*\bb'[vsi]'|\bb'[vsi]'.*\bvolatility\b")
NEEDS = ("provolatile", "Volatility::", "contain_volatile_functions", "FIRST_NORMAL", "FirstNormal", "leakproof", "proisstrict", "proparallel", "func_volatile", "func_parallel", "volatility")   # any of these in the raw text, or the file is skipped
PLANTED = [(G + "lowerv4/src/ops/hashjoin/planted.rs",
            "pub fn acts(c: &Cat, f: u32) -> bool { c.func(f).is_ok_and(|p| p.volatility == Some(b'v')) }\n")]
CLEAN = [(G + "lowerv4/src/ops/hashjoin/clean.rs",
          "// provolatile in a comment is not a test\npub fn acts(e: Effects) -> bool { e.acts }\n")]
