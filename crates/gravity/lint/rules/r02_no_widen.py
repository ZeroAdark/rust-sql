"""R2: a statement-wide `no_widen` ban (PLAN-v5 §3.2; constructs #22). A known-broken form is one named gap,
`Bug(D-id)`, in its family's admit.rs, deleted with the verified root fix."""
import re
from common import G, in_src

ID = "R2"
TITLE = "a no_widen ban or an NW_D* constant"
INSTEAD = "a Bug(D-id) gap in the family's ops/<family>/admit.rs"
HOME = []
SCOPE = in_src
PATTERN = re.compile(r"\bno_widen\b|\bNW_D\d+")
NEEDS = ("no_widen", "NW_D")   # any of these in the raw text, or the file is skipped
PLANTED = [(G + "lowerv4/src/ops/setop/planted.rs", "pub fn no_widen(g: &Graph) -> Option<&'static str> { None }\n")]
CLEAN = [(G + "lowerv4/src/ops/setop/admit.rs", "pub fn gaps(g: &Graph) -> Option<Gap> { Some(Gap::Bug(D2194)) }\n")]
