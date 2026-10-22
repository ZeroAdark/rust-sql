"""C8: GUC, environment and session reads go through inputs (constructs #8, #26): a lowering-time read becomes a recorded
program input that the cache key covers; run time restores PG's own GUC carry. A session thread_local! has no lexical
form apart from scratch ones, so it is counted (report) but not ruled."""
import re
from common import home, in_any, in_src

ID = "C8"
TITLE = "a GUC or environment read outside inputs"
INSTEAD = "gravity_lower::inputs / gravity_exec::inputs (knobs/)"
SCOPE = in_src
_HOME = home("inputs")


def is_home(p):
    return in_any(p, _HOME)


PATTERN = re.compile(r"\bguc_tables::|\bguc_seams::|\bguc::store::|\bvars::\w+\s*\.\s*read\s*\(|\bglobals::work_mem\b"
                     r"|\benv::var(?:_os)?\s*\(|\benv::vars(?:_os)?\s*\(")
NEEDS = ("guc", "vars::", "globals::work_mem", "env::var")   # any of these in the raw text, or the file is skipped
PLANTED = [("crates/gravity/hosts/src/ops/scan/planted.rs",
            "pub fn mb() -> u64 { std::env::var(\"PGRUST_X\").ok().and_then(|v| v.parse().ok()).unwrap_or(0) }\n")]
CLEAN = [("crates/gravity/hosts/src/ops/scan/clean.rs", "pub fn mb(i: &Inputs) -> u64 { i.knob(Knob::ScanMb) }\n"),
         ("crates/gravity/hosts/src/inputs.rs",
          "pub fn mb() -> u64 { std::env::var(\"PGRUST_X\").ok().and_then(|v| v.parse().ok()).unwrap_or(0) }\n")]
