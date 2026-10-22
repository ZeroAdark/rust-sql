//! The diagnostic levers read where they act (phases.rs, owner.rs, front.rs): rows only, so the table stays the census of every knob literal in the crate.

use super::{Kind, Knob};

pub static KNOBS: &[Knob] = &[
    Knob { name: "pgrust.gravity_front_check", kind: Kind::Custom, default: "off", doc: "on: a front-key hit decomposes and keys again and raises Reason::Bug on any difference (admit.rs front_check; read once per GUC change); and every lowering verifies its IR, verify once off (lowerv4 frame/program.rs). On in every harness: e2e, regress, rig's valid run" },
    Knob { name: "pgrust.gravity_progcache_check", kind: Kind::Custom, default: "off", doc: "on (the harnesses): a shared-tier program hit lowers again and raises when its content key or template differs (admit.rs progcache_check)" },
    Knob { name: "pgrust.gravity_shared_programs", kind: Kind::Custom, default: "on", doc: "off|false|0|no: admission neither reads nor publishes the process-wide program tier (admit.rs shared_key; A/Bs of the tier)" },
    Knob { name: "pgrust.gravity_road_off", kind: Kind::Custom, default: "", doc: "twin A/Bs + dev: the roads forced off (id,id,..), read by lowerv4 at every lowering (SET takes effect next statement: a changed value moves the program epoch, hosts residency.rs road_watch, D311); an unknown id is ERROR 22023; REFCELLS VOIDs any run with it set (D255)" },
    Knob { name: "pgrust.gravity_rows_road", kind: Kind::Custom, default: "off", doc: "PGRCSERVE G4's twin lever: on sends every user heap table a statement scans (relid >= FirstNormalObjectId) down the rows road (the table AM's slot scan, hosts ops/scan/am_rows.rs, read serially), the road a front-bearing pgrcolumnar table takes; its other sites over such a table refuse. Read by hosts facts.rs at every decompose; a changed value moves the program epoch (residency.rs road_watch)" },
    Knob { name: "PGRUST_GRAVITY_PHASES", kind: Kind::Env, default: "unset", doc: "1: the per-statement PHASES line (phases.rs; the section driver's switch, read once)" },
    Knob { name: "PGRUST_GRAVITY_SERVE_LEAN", kind: Kind::Env, default: "unset (on)", doc: "0: the serve path's per-statement cuts off (D4601, pglserve), the old paths: a hosted aggregate's 64 home shards made at its arena's start, a parallel run's report census read every statement; the same rows, errors and charges either way (driver env.rs serve_lean, read once per process)" },
    Knob { name: "PGRUST_GRAVITY_AHEAD_TRACE", kind: Kind::Env, default: "unset (off)", doc: "C2 (b), the run-ahead tracer: report = a server log line per run ahead of PostgreSQL's demand without a licence, abort = a panic there (driver ahead_trace.rs reads the mode); either one also makes lowerv4 stamp each pipeline's demand gate (frame/program.rs set_gate_stamps, read once at the lowerer's registration)" },
    Knob { name: "PGRUST_GRAVITY_DIAG", kind: Kind::Env, default: "unset", doc: "1: lowerv4's all-blockers mode (frame/diag.rs, gate.md §16, V4R-109): a refused lowering logs one `gravity: blocker` line per blocker, then refuses as with the mode off; rig.py --diag and gravity-bar-dryrun.sh DIAG=1 only, never a product run (read once, handed to lowerv4 where knobs/lowerer.rs registers it, D542)" },
    Knob { name: "PGRUST_GRAVITY_TIE_BAIL", kind: Kind::Env, default: "unset (on)", doc: "0: a kill switch for diagnosis, not a tuning lever (5c-F6, ops/sort/topn/proto.rs tie_bail; read once; no shipped config sets it): a top-count split (twopass.count) whose pass 1 admits past TWOPASS_TOP_ADMIT_CUT_PCT of its records probes its exact admit set in pass 2 instead of folding every key; the same rows either way" },
    Knob { name: "PGRUST_GRAVITY_RESIDENT_CAP", kind: Kind::Env, default: "unset (RESIDENT_MAX: a slot region's u32 bytes)", doc: "a witness lever, not a tuning one (D3027 (b), ops/sort/merge.rs resident_cap; read once): bytes, lowering (never raising) the cap past which a W > 1 sort's sealed worker runs are put to the slot's stream instead of merged into the leader's run (e2e genfix2_d3027_top_seal_over)" },
    Knob { name: "PGRUST_GRAVITY_HOST_POLL", kind: Kind::Env, default: "unset (on)", doc: "0: the long host loops (a dictionary region's unwrap, a dictionary walk or derived build, the part cache's trim) no longer poll the statement's abort byte (cancelpoll::canceled false): the behaviour before D3887, the poll-gap probe's before-arm (glue.rs poll_gap_probe; read once per process)" },
    Knob { name: "PGRUST_GRAVITY_POLL_GAP_MS", kind: Kind::Env, default: "unset (off)", doc: "D3887's poll-gap probe (cancelpoll): ms; every stretch of a thread inside a kernel call between two abort polls at least this long logs one `poll-gap:` line on stderr with the host-call labels that held it (glue.rs poll_gap_probe; read once per process, at the first statement)" },
    Knob { name: "PGRUST_GRAVITY_STEP_GAP_MS", kind: Kind::Env, default: "unset (cancelpoll STEP_GAP_MS, 2000)", doc: "C9 checker 2, debug builds only (cancelpoll set_step_gap_ms; 0: off): ms; a polled host step's stretch without a poll at least this long logs one `step-gap:` line per step label on stderr (read once per process with the probe; a release build ignores it)" },
    Knob { name: "PGRUST_GRAVITY_CANCEL_AT_STEP", kind: Kind::Env, default: "unset (off)", doc: "C9, a witness lever, never a product setting (cancelpoll set_cancel_at): the start of a host step's label (`fold_`); each polled step whose label starts with it and that opens inside a kernel call sends its statement's backend a SIGINT (pg_cancel_backend's) before its first tick, so an e2e reaches a cancel inside that step with no duration (e2e keyrep_c9_distinct_fold_cancel; read once per process with the probe)" },
    Knob { name: "PGRUST_GRAVITY_POLL_STALL_MS", kind: Kind::Env, default: "unset (off)", doc: "C9, the pollgap gate's born-red lever (cancelpoll set_stall_ms): ms; the first kernel call of the process spins this long with no abort poll, labelled `cancelpoll injected stall`, so the probe logs one gap the gate keys as a fresh site (read once per process with the probe)" },
    Knob { name: "PGRUST_GRAVITY_SMALL_SORT", kind: Kind::Env, default: "unset (on)", doc: "0: the kernels' small sort road off (glue.rs sets kernels ops/sort/rowsort.rs set_small_sort once per process, at the first statement): a sort run or a group emit's sort of at most SMALL_SORT_ROWS (256) rows orders through the full road's grant blocks and polled sort again; both roads put the same rows in the same order (an A/B and rollback lever)" },
    Knob { name: "PGRUST_GRAVITY_SHED_ONCE", kind: Kind::Env, default: "unset (on)", doc: "0: a parked Node is shed again at every put over the park budget (driver park.rs set_shed_once, glue.rs sets it once per process at the first statement); on, a parked Node whose regions went back is passed by the over-budget loop until it is taken (a parked Node runs nothing, so a second shed frees nothing; an A/B and rollback lever)" },
    Knob { name: "PGRUST_GRAVITY_POOLED_MEMO", kind: Kind::Env, default: "unset (on)", doc: "0: a context block asks the block source whether it is pooled at every reset and drop (mcx heap.rs set_pooled_memo, glue.rs sets it once per process at the first statement); on, a block keeps the answer after its first ask once the source is installed (the allocator that served a block owns it until it is freed; a per-row context's reset asked the source's range lookup every row; an A/B and rollback lever)" },
    Knob { name: "PGRUST_GRAVITY_POOLED_BIT", kind: Kind::Env, default: "unset (on)", doc: "0: a context block's free, reset and resize ask the block source whether it is pooled (the source's range lookup; mcx heap.rs set_pooled_bit) and its pooled answer is the memo's (PGRUST_GRAVITY_POOLED_MEMO), and a program arena's reset asks pagepool::taken (gravity_program arena.rs set_pooled_bit); glue.rs sets both once per process at the first statement. On, a block and an AllocSet / bump dedicated chunk carry which allocator served them from their allocation once the source is installed (the source's take served or refused it; a resize records the new chunk's), an arena whether the pool's road served its block (header word 2's top bits), and they read that instead (debug builds check it against the lookup; an A/B and rollback lever)" },
    Knob { name: "PGRUST_GRAVITY_SEND_GROW", kind: Kind::Env, default: "unset (on)", doc: "0: the protocol send buffer flushes whenever its 8 KiB fill, as C's (pqcomm set_send_grow through gravity_wire, glue.rs sets it once per process at the first statement); on, a buffer a message fills grows once to 64 KiB first, so a result of tens of kilobytes leaves in one send at the protocol's flush (sysbench oltp_read_only's 100-row ranges sent 2 to 3 sends each; 56 KiB more per session that sent such a result; an A/B and rollback lever)" },
    Knob { name: "PGRUST_GRAVITY_SEND_INTO", kind: Kind::Env, default: "unset (on)", doc: "0: printtup's binary text, bpchar and varchar columns call their send function per row (a bytea made in the per-row context, copied into the DataRow, the context reset) (fmgr_core set_send_into, glue.rs sets it once per process at the first statement); on, such a column's inline value goes straight into the DataRow when no encoding conversion applies, as the text format's output columns already do (an A/B and rollback lever)" },
    Knob { name: "PGRUST_GRAVITY_PARK_PLANS", kind: Kind::Env, default: "unset (on)", doc: "0: a static statement level parks at most 64 Nodes (exec lib.rs PARK_NODES; read at each level's build, lib.rs park_nodes); on, as many as the plans it keeps (MAX_PLANS, 256; the e2e memo-slots knob never takes it under 64), the park's byte cap (1/11 of the pool) unchanged: a backend with more than 64 statement shapes no longer rebuilds and drops a Node at every count eviction (an A/B and rollback lever)" },
    Knob { name: "PGRUST_GRAVITY_FRONT_RUNS", kind: Kind::Env, default: "unset (on)", doc: "0: code.front_part lowers without its run cache (lowerv4 dict/codefront.rs set_runs; read once, handed to lowerv4 where knobs/lowerer.rs registers it): every row whose code is past the front's capacity takes a hashed upsert again; on, a row of the code the last such upsert took folds into that record with no image, hash or probe, so a run of one code costs one upsert as under the run carry the front shuts out (an A/B and rollback lever)" },
    Knob { name: "PGRUST_GRAVITY_SEMI_KEYS", kind: Kind::Env, default: "unset (on)", doc: "D4904, 0: a SEMI / ANTI hash build keeps every record (lowerv4 ops/hashjoin/strategies/dedup.rs set_keys; read once, handed to lowerv4 where knobs/lowerer.rs registers it); on, a build whose probe reads no build column past its key test drops each record whose key image another holds before it grows or spills, so a build of repeating keys fits its grant where it spilled (an A/B and rollback lever)" },
    Knob { name: "PGRUST_GRAVITY_PARK_INDEX", kind: Kind::Env, default: "unset (on)", doc: "P2 W1, 0: a park's take, put and eviction walk every slot and its release pass visits every parked Node, as before (driver park.rs set_park_index, glue.rs sets it once per process at the first statement); on, an index beside the slots (a key -> newest-entry table, the LRU list, the free-slot bits) answers them once a park has grown PARK_INDEX_FROM (16) slots, and the release pass passes a Node whose sources read none of the released relations. The same slot, entry and Node either way (an A/B and rollback lever)" },
    Knob { name: "PGRUST_GRAVITY_PARK_INDEX_CHECK", kind: Kind::Env, default: "unset (off; on in a debug build)", doc: "P2 W1, 1: an indexed park also walks its slots at every take, put, eviction and release and compares (driver park.rs set_park_index, glue.rs once per process); a mismatch is a 'gravity: park: index check mismatch' LOG line, a count (park::index_mismatches) and a debug build's panic; the walk's answer is the one used" },
];

/// `PGRUST_GRAVITY_HOST_POLL` (glue.rs reads it once per process): the host loops' polls, on unless `0` / `off`.
pub(crate) fn host_polls() -> bool {
    !matches!(super::env("PGRUST_GRAVITY_HOST_POLL").as_deref().and_then(|v| v.to_str()), Some("0" | "off"))
}

/// `PGRUST_GRAVITY_POLL_GAP_MS` (glue.rs reads it once per process): the poll-gap probe's threshold, ms.
pub(crate) fn poll_gap_ms() -> Option<u64> {
    super::env_u64("PGRUST_GRAVITY_POLL_GAP_MS")
}

/// `PGRUST_GRAVITY_SMALL_SORT` (glue.rs reads it once per process): the kernels' small sort road, on unless `0` / `off`.
pub(crate) fn small_sort() -> bool {
    !matches!(super::env("PGRUST_GRAVITY_SMALL_SORT").as_deref().and_then(|v| v.to_str()).map(str::trim), Some("0" | "off"))
}

/// `PGRUST_GRAVITY_SHED_ONCE` (glue.rs reads it once per process): a parked Node shed at most once while parked, on unless `0` / `off`.
pub(crate) fn shed_once() -> bool {
    !matches!(super::env("PGRUST_GRAVITY_SHED_ONCE").as_deref().and_then(|v| v.to_str()).map(str::trim), Some("0" | "off"))
}

/// `PGRUST_GRAVITY_POOLED_MEMO` (glue.rs reads it once per process): a context block keeps its pooled answer, on unless `0` / `off`.
pub(crate) fn pooled_memo() -> bool {
    !matches!(super::env("PGRUST_GRAVITY_POOLED_MEMO").as_deref().and_then(|v| v.to_str()).map(str::trim), Some("0" | "off"))
}

/// `PGRUST_GRAVITY_POOLED_BIT` (glue.rs reads it once per process): a context block carries the allocator that served it, on unless `0` / `off`.
pub(crate) fn pooled_bit() -> bool {
    !matches!(super::env("PGRUST_GRAVITY_POOLED_BIT").as_deref().and_then(|v| v.to_str()).map(str::trim), Some("0" | "off"))
}

/// `PGRUST_GRAVITY_SEND_GROW` (glue.rs reads it once per process): the send buffer grows to 64 KiB before it flushes when full, on unless `0` / `off`.
pub(crate) fn send_grow() -> bool {
    !matches!(super::env("PGRUST_GRAVITY_SEND_GROW").as_deref().and_then(|v| v.to_str()).map(str::trim), Some("0" | "off"))
}

/// `PGRUST_GRAVITY_SEND_INTO` (glue.rs reads it once per process): printtup's binary text columns render into the DataRow, on unless `0` / `off`.
pub(crate) fn send_into() -> bool {
    !matches!(super::env("PGRUST_GRAVITY_SEND_INTO").as_deref().and_then(|v| v.to_str()).map(str::trim), Some("0" | "off"))
}

/// `PGRUST_GRAVITY_PARK_PLANS` (exec lib.rs reads it at each static level's build): the park holds as many Nodes as the
/// level keeps plans, on unless `0` / `off`.
pub(crate) fn park_plans() -> bool {
    !matches!(super::env("PGRUST_GRAVITY_PARK_PLANS").as_deref().and_then(|v| v.to_str()).map(str::trim), Some("0" | "off"))
}

/// `PGRUST_GRAVITY_FRONT_RUNS` (read once, at the lowerer's registration): code.front_part's run cache, on unless `0` / `off`.
pub(crate) fn front_runs() -> bool {
    !matches!(super::env("PGRUST_GRAVITY_FRONT_RUNS").as_deref().and_then(|v| v.to_str()).map(str::trim), Some("0" | "off"))
}

/// `PGRUST_GRAVITY_SEMI_KEYS` (knobs/lowerer.rs reads it once, where it registers the lowerer): a SEMI / ANTI build keeps
/// one record per key, on unless `0` / `off`.
pub(crate) fn semi_keys() -> bool {
    !matches!(super::env("PGRUST_GRAVITY_SEMI_KEYS").as_deref().and_then(|v| v.to_str()).map(str::trim), Some("0" | "off"))
}

/// `PGRUST_GRAVITY_PARK_INDEX` (glue.rs reads it once per process): the park's index, on unless `0` / `off`.
pub(crate) fn park_index() -> bool {
    !matches!(super::env("PGRUST_GRAVITY_PARK_INDEX").as_deref().and_then(|v| v.to_str()).map(str::trim), Some("0" | "off"))
}

/// `PGRUST_GRAVITY_PARK_INDEX_CHECK` (glue.rs reads it once per process): the index's check, off unless `1` / `on`.
pub(crate) fn park_index_check() -> bool {
    matches!(super::env("PGRUST_GRAVITY_PARK_INDEX_CHECK").as_deref().and_then(|v| v.to_str()).map(str::trim), Some("1" | "on"))
}

/// `PGRUST_GRAVITY_POLL_STALL_MS` (glue.rs reads it once per process): the pollgap gate's injected stall, ms.
pub(crate) fn poll_stall_ms() -> Option<u64> {
    super::env_u64("PGRUST_GRAVITY_POLL_STALL_MS")
}

/// `PGRUST_GRAVITY_CANCEL_AT_STEP` (glue.rs reads it once per process): the start of the witness lever's step label;
/// unset or empty: off.
pub(crate) fn cancel_at_step() -> Option<String> {
    super::env("PGRUST_GRAVITY_CANCEL_AT_STEP").and_then(|v| v.into_string().ok()).filter(|v| !v.is_empty())
}

/// `PGRUST_GRAVITY_STEP_GAP_MS` (glue.rs reads it once per process): checker 2's threshold, ms (debug builds).
pub(crate) fn step_gap_ms() -> Option<u64> {
    super::env_u64("PGRUST_GRAVITY_STEP_GAP_MS")
}

/// `PGRUST_GRAVITY_RESIDENT_CAP` (ops/sort/merge.rs reads it once): the witness lever's bytes.
pub(crate) fn resident_cap() -> Option<u64> {
    super::env_u64("PGRUST_GRAVITY_RESIDENT_CAP")
}

/// `PGRUST_GRAVITY_AHEAD_TRACE` (read once, at the lowerer's registration): the run-ahead tracer is on (`report` or
/// `abort`), so lowerv4 stamps the pipelines' demand gates it reads.
pub(crate) fn ahead_trace() -> bool {
    matches!(super::env("PGRUST_GRAVITY_AHEAD_TRACE").as_deref().and_then(|v| v.to_str()), Some("report" | "abort"))
}

/// `PGRUST_GRAVITY_DIAG` (read once, at the lowerer's registration): lowerv4's all-blockers mode.
pub(crate) fn all_blockers() -> bool {
    super::env_on("PGRUST_GRAVITY_DIAG")
}
