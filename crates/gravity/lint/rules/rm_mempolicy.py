"""RM: a memory decision outside the memory policy (the memory core's design rev 4 §2: a memory decision anywhere else fails
review once step 2 (C1) lands). The home is `mempolicy` (crates/_support/mempolicy/src/): every memory threshold, cap,
size, order and wait is a function or a constant there that the mechanisms ask. RM reads the memory files (`MEMORY`: the
grant pool, the region roads and sizes, the depots and parks, the reclaimer, the broker, the reuse pool, pgrc's part cache
and registry, the memory governor, the memory runtime, the idle release and the bulk-write governor), Gravity's and the
named non-Gravity ones, and finds by construct:
- a memory knob: a `const` / `static` with a numeric literal value (or a `Duration` literal) whose name carries a bound,
  size or time word (`KNOB_WORD`), charged to its name;
- a memory comparison: a code line that reads the process's memory state (`READ`: the governor's bar, limit, projection
  and probes, the grant pool's held / capacity, the context footprint, the part cache's bytes and budget, a depot's bytes,
  the ledger's rows, the /proc and cgroup memory files) or a policy knob (`consts::get`: the policy's number compared
  outside the policy is its decision made outside it, D3644 (b)) and compares it (`<`, `>`, `<=`, `>=`, `.min(`, `.max(`,
  `.clamp(`; a clamp to 0 is a count's floor, not a decision), charged to the innermost function.
A construct that is a count or a mechanism, not a decision, is listed in `NOT_DECISIONS` here with why, reviewed with the
rule. Any other one outside the home needs a row in lint/MEMPOLICY-DEBT (a directory, one row per file): path, name, owner
lane, the step of the memory core that removes it (step3 .. step7, its §7 order) and what it decides; an unlisted one
FAILS. On a landing (--base) a row the parent lacks fails (the list only shrinks), unless it replaces a row of the same
name the landing removes (a certified move), or the parent has no ledger yet. A row whose construct is gone is a note
(delete it)."""
import re

from common import G, ledger_loc, Src, generated, in_any, walk_sources
from rl_lifetime import functions

ID = "RM"
TITLE = "a memory decision outside the memory policy"
INSTEAD = "a mempolicy function or constant the mechanism asks (crates/_support/mempolicy); not yet moved: a " \
          "lint/MEMPOLICY-DEBT row naming the step that removes it"
GLOBAL = True
DEBT = G + "lint/MEMPOLICY-DEBT"
HOME = ["crates/_support/mempolicy/src/"]
# The memory files: Gravity's (also in walk_sources) and the named non-Gravity ones RM reads itself.
MEMORY = [G + "driver/src/mem/" + f for f in ("grant.rs", "region.rs", "grow.rs", "reclaim.rs", "budget.rs", "arena.rs", "resident.rs",
                                              "reap.rs", "feedback.rs", "spill_host.rs")] + [
    G + "driver/src/park.rs", G + "driver/src/slots.rs", G + "hosts/src/membroker/", G + "hosts/src/dict/text_hosts.rs",
    G + "hosts/src/ops/scan/pgrc/residency.rs", G + "exec/src/inputs/knobs/grant.rs", G + "exec/src/runner/portal.rs"]
OUTSIDE = ["crates/_support/mcx/src/governor.rs", "crates/backend/postmaster/memruntime/src/lib.rs",
           "crates/backend/postmaster/memruntime/src/loops.rs", "crates/backend/tcop/postgres/src/passivate.rs",
           "crates/backend/storage/file/write_governor/src/lib.rs",
           "crates/backend/access/pgrcolumnar/pgrc_read/src/partcache.rs",
           "crates/backend/access/pgrcolumnar/pgrc_am/src/inval.rs"]
STEPS = {"step3", "step4", "step5", "step6", "step7"}

# (path, name): why it is a count or a mechanism, not a decision
NOT_DECISIONS = {
    ("crates/_support/mcx/src/governor.rs", "LEDGER_SLACK"):
        "a count's batching: a thread adds its block charges to its statement ledger's shared total once they net this much",
    ("crates/_support/mcx/src/governor.rs", "admit"):
        "the allocation hot path's pre-solved form of mempolicy::admit::under_bar (publish derives FOOTPRINT_LIMIT from the "
        "bar); past it admit_slow asks the policy",
    ("crates/gravity/hosts/src/ops/scan/pgrc/residency.rs", "trim_part_cache"):
        "a trim's no-op check against the policy's target (mempolicy::cache::part_cache_trim_target)",
}

KNOB_WORD = re.compile(r"(?:^|_)(?:MAX|MIN|CAP|LIMIT|THRESH[A-Z]*|FLOOR|CEIL[A-Z]*|BUDGET|TARGET|RATIO|FACTOR|FRACTION|PCT|PPM"
                       r"|MS|SECS?|WAIT|DELAY|PERIOD|CADENCE|BYTES|SLACK|KEEP|GRACE|STALL|SETTLE|TICK|ALLOWANCE)(?:_|$)")
KNOB = re.compile(r"\b(?:const|static)\s+([A-Z][A-Z0-9_]*)\s*:\s*(?:u8|u16|u32|u64|u128|usize|i8|i16|i32|i64|isize|f32|f64"
                  r"|(?:core::time::|std::time::)?Duration)\s*=\s*(?:[0-9][0-9_.]*(?:\s*(?:<<|\*|/|\+|-)\s*[0-9_.]+)*"
                  r"|(?:core::time::|std::time::)?Duration::from_(?:millis|secs|micros)\(\s*[0-9_]+\s*\))\s*;")
READ = re.compile(r"governor::(?:bar|limit|projected|probe_\w+|resident\w*|cache_bytes|allocator_\w+)\s*\(|\bPOOL\.(?:held|capacity"
                  r"|inflight|kept|small_held|reserve|max_inflight)\s*\(|global_footprint::(?:bytes|thread_current)\s*\("
                  r"|partcache::(?:bytes|budget)\s*\(|\.stats\(\)\.bytes|memledger::(?:account_row|snapshot|rows|region_mapped_bytes)\b"
                  r"|/proc/self/(?:status|smaps|statm)|memory\.(?:max|current|high|stat)\b|\b(?:RssAnon|VmRSS|MemAvailable|MemTotal)\b"
                  r"|\bconsts::get(?:_ms)?\s*\(")
CMP = re.compile(r"\s(?:<|>|<=|>=)\s|\.(?:min|max)\((?!\s*0\s*\))|\.clamp\(")
_NEEDS = re.compile(r"\b(?:const|static)\b|governor::|POOL\.|global_footprint::|partcache::|\.stats\(\)|memledger::|/proc/self/|memory\."
                    r"|RssAnon|VmRSS|MemAvailable|MemTotal|consts::get")


def is_home(p):
    return in_any(p, HOME)


def decisions(src):
    """-> {(name, line)} of the knobs `src` defines and the functions holding a memory comparison (see the module doc)."""
    out = set()
    code = src.joined("code")
    for m in KNOB.finditer(code):
        if KNOB_WORD.search(m.group(1)):
            out.add((m.group(1), code.count("\n", 0, m.start()) + 1))
    fns, off = functions(src), 0
    for line in src.code:
        if READ.search(line) and CMP.search(line):
            inner = [(n, ln, b - a) for n, ln, (a, b) in fns if a <= off < b]
            if inner:
                n, ln, _ = min(inner, key=lambda x: x[2])
                out.add((n, ln))
        off += len(line) + 1
    return out


def memory_files(ctx):
    seen = [p for p in walk_sources(ctx.root) if in_any(p, MEMORY)]
    return seen + [p for p in OUTSIDE if ctx.read(p) is not None]


def parse_debt(text):
    """-> ({(path, name): (line, owner, step)}, [(line, problem)])."""
    rows, bad = {}, []
    for n, raw in enumerate(text.split("\n"), 1):
        s = raw.split("#", 1)[0].rstrip()
        if not s.strip():
            continue
        f = s.split("\t")
        if len(f) < 5 or not all(x.strip() for x in f[:5]):
            bad.append((n, "a row is path<TAB>name<TAB>owner lane<TAB>step<TAB>what it decides"))
            continue
        step = f[3].strip()
        if step not in STEPS:
            bad.append((n, f"step {step!r} is not the memory core step that removes it ({', '.join(sorted(STEPS))})"))
            continue
        key = (f[0].strip(), f[1].strip())
        if key in rows:
            bad.append((n, f"duplicate row {key[0]} {key[1]}"))
        rows[key] = (n, f[2].strip(), step)
    return rows, bad


def scan(ctx):
    """-> {(path, name): line} over the memory files, NOT_DECISIONS left out."""
    hits = {}
    for p in memory_files(ctx):
        if is_home(p):
            continue
        t = ctx.read(p)
        if t is None or generated(t) or not _NEEDS.search(t):
            continue
        for name, line in decisions(Src(p, t)):
            if (p, name) not in NOT_DECISIONS:
                hits.setdefault((p, name), line)
    return hits


def check_tree(ctx):
    out = []
    text, at = ctx.ledger(DEBT)
    rows, bad = parse_debt(text) if text is not None else ({}, [])
    out += [(*at(n), note) for n, note in bad]
    hits = scan(ctx)
    for (p, name), n in sorted(hits.items()):
        if (p, name) not in rows:
            out.append((p, n, f"`{name}` decides memory outside the memory policy; instead: {INSTEAD}"))
    stale = [f"{ledger_loc(at, n)}: {p} `{name}` ({owner}) is no longer found: delete the row"
             for (p, name), (n, owner, _) in sorted(rows.items(), key=lambda kv: kv[1][0]) if (p, name) not in hits]
    ctx.notes += [f"RM: {s}" for s in stale]
    base_text = ctx.base_ledger(DEBT) if ctx.base_ledger else None
    if base_text is not None:
        base, _ = parse_debt(base_text)
        removed = {bn for bp, bn in base if (bp, bn) not in rows}
        for (p, name), (n, owner, _) in sorted(rows.items(), key=lambda kv: kv[1][0]):
            if (p, name) not in base and name not in removed:
                out.append((*at(n), f"{p} `{name}` ({owner}) added: the list only shrinks (decide it in mempolicy)"))
    return out


# the planted failure is an unlisted knob in a memory file; the clean one asks the policy
PLANTED_TOML = (G + "driver/src/park.rs", "pub const PARK_KEEP_BYTES: usize = 2 << 30;\n")
CLEAN_TOML = (G + "driver/src/park.rs", "pub fn park_bytes() -> usize {\n    mempolicy::keep::park_bytes(POOL.capacity())\n}\n")
MORE_PLANTED_TOML = [(G + "driver/src/slots.rs",
                      "fn over() -> bool {\n    crate::slots::DEPOT.stats().bytes > POOL.capacity() as usize / 3\n}\n"),
                     # D3644 (b)'s first form: the work floor compared in the driver, not asked of the policy
                     (G + "driver/src/mem/spill_host.rs",
                      "unsafe fn work_take(ctx: *mut Ctx, bytes: usize) -> i64 {\n"
                      "    if (bytes as u64) < mempolicy::consts::get(mempolicy::consts::K::WorkFloor) {\n        return 1;\n    }\n"
                      "    0\n}\n")]
MORE_CLEAN_TOML = [(G + "driver/src/slots.rs", "fn count(n: isize) -> usize {\n    global_footprint::bytes().max(0) as usize + n as usize\n}\n"),
                   (G + "driver/src/mem/spill_host.rs",
                    "unsafe fn work_take(ctx: *mut Ctx, bytes: usize) -> i64 {\n"
                    "    if !mempolicy::grow::work_from_grant(bytes as u64) {\n        return 1;\n    }\n    0\n}\n")]
