"""C32: the one hand-off to the row executor (constructs #32): calls into the previous executor hooks appear only in
admit::entry (exec/src/admit/entry; the entry crate until it folds into exec)."""
import re
from common import home, in_any, in_src

ID = "C32"
TITLE = "a call into the row executor outside admit::entry"
INSTEAD = "gravity_exec::admit::entry (the prologue and the one hand-off)"
SCOPE = in_src
_HOME = home("entry")


def is_home(p):
    return in_any(p, _HOME)


PATTERN = re.compile(r"\bprev\s*\(\s*\)\s*\.\s*\w+|\bstandard_[Ee]xecutor_?\w+\s*\(")
NEEDS = ("prev", "standard_")   # any of these in the raw text, or the file is skipped
PLANTED = [("crates/gravity/exec/src/ops/lockrows/planted.rs", "pub fn run(qd: &mut QueryDesc) { (prev().executor_run)(qd) }\n")]
CLEAN = [("crates/gravity/exec/src/ops/lockrows/clean.rs", "pub fn run(qd: &mut QueryDesc) { admit::entry::hand_off(qd) }\n")]
