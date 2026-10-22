"""C14: root memory contexts and spill files are created only in mem (the one spill, mem::spill).
Child contexts and the spill set's own API are free to use. Registering the process's spill factory
(mem::spill::install_spill_factory) creates nothing, so only the creation call spill_factory() is matched."""
import re
from common import home, in_any, in_src

ID = "C14"
TITLE = "a root memory context or a spill file created outside mem"
INSTEAD = "gravity_driver::mem (a scoped context per re-open; mem::spill)"
SCOPE = in_src
_HOME = home("mem")


def is_home(p):
    return in_any(p, _HOME)


PATTERN = re.compile(r"\bMemoryContext::new\s*\(|\bspill_factory\s*\(\s*\)")
NEEDS = ("MemoryContext::new", "spill_factory")   # any of these in the raw text, or the file is skipped
PLANTED = [("crates/gravity/exec/src/ops/memoize/planted.rs",
            "pub fn ctx() -> Box<MemoryContext> { Box::new(MemoryContext::new(\"GravityMemo\")) }\n"),
           ("crates/gravity/pool/src/planted.rs",
            "pub fn set() -> Box<dyn SpillSet> { spill_factory().unwrap().create().unwrap() }\n")]
CLEAN = [("crates/gravity/exec/src/ops/memoize/clean.rs", "pub fn ctx(m: &mut Mem) -> Scoped<'_> { m.scope(\"memo\") }\n"),
         ("crates/gravity/pool/src/clean.rs",
          "pub fn install() -> bool { gravity_driver::mem::spill::install_spill_factory(&SPILL_SETS) }\n")]
