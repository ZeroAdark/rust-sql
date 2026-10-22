//! PostgreSQL's format and protocol limits (contract::layout): a size the runtime takes from
//! PostgreSQL, each named and valued as PostgreSQL's constant, so a row or message is bounded as PostgreSQL bounds it.
//! Below them, Gravity's own format and protocol capacities that have no PostgreSQL constant, each saying so.

/// C6 (e), the one raise of a limit user input reaches: PostgreSQL's ERROR 54000 (program_limit_exceeded) as a `PgError`
/// panic payload, never a plain panic. Since D2933 a panic whose payload is not a `PgError` aborts the server (tcop's
/// main_loop turns only a `PgError` into an ERROR). Inside a host's pad the payload is parked and resumed when the JIT'd
/// entry returns (`status::unwind`), so the statement ends with this ERROR and the server lives. The message reads
/// `gravity: <what>`. A broken invariant stays a plain panic: lint RL2 lists every limit-named panic, and exempt.py says
/// why each bound left a panic holds.
#[cold]
#[inline(never)]
pub fn exceeded(what: core::fmt::Arguments<'_>) -> ! {
    extern crate std;
    let e = types_error::PgError::error(std::format!("gravity: {what}")); // alloc: Cold
    std::panic::panic_any(e.with_sqlstate(types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED))
}

/// Columns in a tuple, and so in a target list (`MaxTupleAttributeNumber`, access/htup_details.h).
pub const MAX_TUPLE_ATTRIBUTE_NUMBER: usize = 1664;

/// A jsonb container's entry stride (`JB_OFFSET_STRIDE`, utils/jsonb.h): every `JB_OFFSET_STRIDE`-th JEntry stores its
/// end offset and the others their length, so a container of fewer entries stores an offset only in entry 0.
pub const JB_OFFSET_STRIDE: usize = 32;

/// The most bytes one input byte becomes in an encoding conversion (`MAX_CONVERSION_GROWTH`, mb/pg_wchar.h).
pub const MAX_CONVERSION_GROWTH: usize = 4;

/// The most 8-byte words a hash join key's image takes in a build record (lowerv4 ops/hashjoin keys.rs refuses a wider
/// key; the build and spill runtimes copy a record's image into a buffer of this many words). A Gravity format capacity
/// with no PostgreSQL constant.
pub const JOIN_KEY_IMAGE_WORDS: usize = 16;

/// The most runs one merge reads at once (`MAXORDER`, utils/sort/tuplesort.c): a merge opens a reader per run, so more
/// runs than this merge in passes first, as tuplesort's tapes do (kernels ops/sort/runmerge.rs `fan_in`, D1494).
pub const MAXORDER: usize = 500;

/// A variable-width sort run's least merge buffer in bytes (kernels ops/sort/runmerge.rs `min_buf` / `fan_in`, D1327): a
/// longer record grows its run's buffer past the region. It moves no answer, only how many runs one merge pass reads.
/// CHOSEN, not measured: 1 KiB, an eighth of the block (BLCKSZ) each tuplesort tape buffer takes, so a small work_mem
/// region still merges tens of runs a pass.
pub const SORT_VAR_MIN_BUF: usize = 1 << 10;

/// The most pairs (exec params) one InitPlan's Init cell holds: the Init proto's pair count (kernels ops/subplan
/// entries.rs `init_pairs`, its arg's low word) and exec's cell (ops/subplan/initplan/init.rs `lay`, sized per Init:
/// `init_pair(n)`). PostgreSQL's setParam list has no bound below a target list's, so it is `MAX_TUPLE_ATTRIBUTE_NUMBER`
/// (D2799: the old 64 refused a 65-column ROWCOMPARE / MULTIEXPR InitPlan where PostgreSQL answers). The arg's low word
/// keeps the count below its flag bits (`INIT_EXISTS`, bit 28).
pub const INIT_MAX_PAIRS: usize = MAX_TUPLE_ATTRIBUTE_NUMBER;

/// D2998: bit 28 of an Init proto's `arg` (kernels entries.rs `init_pairs` masks it): an EXISTS InitPlan, whose own pipeline
/// may run as a section (gravity_parallel `nested_section`): its rows only store true into the cell and stop the claims, and
/// the runtime resets the cell to false before each run (exec initplan raw.rs `ensure`), so no worker's end stores false.
pub const INIT_EXISTS: u64 = 1 << 28;

/// The most keys the planning fact `Order` holds inline (decompose graph.rs `Order.keys` / `exprs` / `fams`): a longer
/// order is cut to its first keys, and a prefix of an order is still an order. A Gravity format capacity with no
/// PostgreSQL constant (a sort's key list has no bound below a target list's).
pub const MAX_ORDER_KEYS: usize = 8;

/// The most sort words a group emit descriptor holds in its fixed part (kernels table/emit.rs: `DESC_SORT` ..
/// `DESC_PROCS`, then as many comparator words): a sorted group by more keys lays its sort words on past `DESC_PROCS`
/// (`DESC_PROCS_AT_SHIFT`), so no key count is refused at it. A Gravity format capacity with no PostgreSQL constant.
pub const DESC_MAX_SORT: usize = 8;

/// The largest fixed scale the numeric aggregate register takes (decompose ops/agg: `SlotKind::SumNum` / `AvgNum`, a
/// mantissa at one scale, and its narrow / wide forms): 10^18 is the largest power of ten an i64 holds (10^18 < 2^63).
/// An argument with a larger scale takes another register or folds its datums. A Gravity format capacity with no
/// PostgreSQL constant (`NUMERIC_MAX_SCALE`, utils/numeric.h, is 1000).
pub const MAX_NUM_SCALE: u32 = 18;

/// The most column equalities the planning fact `Order` holds inline (decompose graph.rs `Order.eqs`): one past it is
/// not recorded, and an order never over-claims. A Gravity format capacity with no PostgreSQL constant.
pub const MAX_ORDER_EQS: usize = 8;

/// The pattern program's capacities (dict regexp/program.rs `Program`, which the per-row tier under RE2 runs too, and
/// regexp.rs `Template`): its ops, its literal bytes and a replacement's pieces. A pattern past one compiles to `None`
/// and takes RE2 or PostgreSQL's engine (a replacement past one stays on the function manager), so none moves an
/// answer. Gravity format capacities with no PostgreSQL constant. CHOSEN, not measured: they size the program's fixed
/// arrays.
pub const REGEXP_MAX_OPS: usize = 32;
pub const REGEXP_MAX_LITS: usize = 256;
pub const REGEXP_MAX_PIECES: usize = 16;

/// The most capture groups the pattern program keeps (dict regexp/program.rs `Program`): a replacement names groups `\1`
/// .. `\9` only (varlena.c `appendStringInfoRegexpSubstr`, which reads one digit after the backslash), so the program
/// keeps as many; a pattern with more compiles to `None` and takes RE2 or PostgreSQL's engine.
pub const REGEXP_MAX_GROUPS: usize = 9;

/// The deepest call nesting the tier-0 IR interpreter runs (gravity_interp `MAX_DEPTH`: a deeper call ends the run with
/// `Trap::StackOverflow`). A Gravity capacity with no PostgreSQL constant (PostgreSQL bounds recursion by
/// `max_stack_depth`, a byte budget, not a frame count). CHOSEN, not measured: it keeps the interpreter's own recursion
/// inside the native stack / a deeper call stops the interpreter.
pub const INTERP_MAX_DEPTH: usize = 512;

/// The deepest predecessor walk of gravity_ir `promote` (the lowerer's counter promotion, frame instr.rs): reading a
/// counter's content recurses once per block between the read and the content's definition, and a walk deeper than this
/// is `Err(TooDeep)`, after which the function keeps its counters in memory (C6 (b)). A Gravity capacity with no
/// PostgreSQL constant. CHOSEN, not measured: well inside a backend thread's stack (max_stack_depth plus 8 MiB); the ir
/// test `a_chain_of_blocks_promotes_or_stops_at_the_read_depth` walks it on an 8 MiB thread in a debug build.
pub const PROMOTE_MAX_READ_DEPTH: u32 = 2048;

/// The bytes of one chunk of the tier-0 IR interpreter's per-thread frame stack (gravity_interp `Stack`, D3229: the frames'
/// stack slots, kept at one address call after call); a frame larger than this gets a chunk of its own size. A Gravity
/// capacity with no PostgreSQL constant. CHOSEN, not measured: it moves no answer, only how often a deep or wide call path
/// takes a new chunk (each kept for the thread's life).
pub const INTERP_STACK_CHUNK: usize = 64 << 10;

/// The rows the plain `count(DISTINCT x)` window entry compacts per pass (kernels ops/group entries_distinct
/// `agg_distinct_window`, its fixed candidate array): the candidates stay in L1 between the compaction and the scatter. A
/// Gravity capacity with no PostgreSQL constant. CHOSEN, not measured: a chunk only; each chunk's surviving keys are
/// upserted as the row loop upserts them, so it moves no answer.
pub const DISTINCT_WINDOW_CHUNK: usize = 1024;

/// The heap fetch site's least varlena arena block in bytes (hosts ops/sort/heap_fetch.rs): an image longer than the
/// block's room takes a fresh block of its own length plus this. A Gravity capacity with no PostgreSQL constant. CHOSEN,
/// not measured: 64 KiB, the pgrc granule source's first arena block; it moves no answer, only how often a request's
/// copies take a fresh block.
pub const HEAP_FETCH_ARENA_MIN: usize = 64 << 10;

/// D3267: a hosted DISTINCT / ORDER BY aggregate's run chunk in words, the first and the largest (hosts
/// ops/group/raw/distinct.rs `room`: each chunk doubles the one before it up to the largest). A Gravity format capacity with
/// no PostgreSQL constant. CHOSEN, not measured: the largest, 8 MiB, keeps every allocation of a run of any length far
/// below MaxAllocSize (1 GiB - 1, mcxt.c), whose ceiling a single doubled run met at 2^26 rows; the first, 32 words, is
/// about the old run's first allocation (16 rows of one input), so a small group's run costs what it did.
pub const DISTINCT_RUN_CHUNK_MIN_WORDS: usize = 32;
pub const DISTINCT_RUN_CHUNK_MAX_WORDS: usize = 1 << 20;

/// D3267: a mapping of the hosts' mapped road in bytes, the first and the largest (hosts ops/shared/mapped.rs `Road::carve`:
/// each later mapping is the bytes mapped so far, up to the largest, or one item's size when larger). A Gravity capacity
/// with no PostgreSQL constant. CHOSEN, not measured: 8 MiB first, so a run just past work_mem maps little; 256 MiB at
/// most, so a 100 M-row run takes tens of mappings, not thousands.
pub const MAPPED_ROAD_MIN_BYTES: usize = 8 << 20;
pub const MAPPED_ROAD_MAX_BYTES: usize = 256 << 20;

/// The bytes a statement level's runner plan scratch keeps between its parallel runs (exec runner plan.rs `PlanScratch`,
/// a named session context row): past them the vectors and their context go after the run. A Gravity capacity with no
/// PostgreSQL constant. CHOSEN, not measured: a plan of hundreds of pipelines and words fits, and one 8 KiB block is
/// the common case, so a session's idle memory never holds more for it.
pub const PLAN_SCRATCH_MAX_BYTES: usize = 64 << 10;

/// D3336 (kernels table/emit.rs `agg_merge_runs_part`, lowerv4 road emit.parmerge): at most this many key ranges of a
/// run table's parallel merge: the length of the splitter array the merge's shared state carries (a format bound, not a
/// tuned speed).
pub const PAR_MERGE_PARTS_MAX: usize = 256;

/// D3336: under this many records the reader's one thread merges the runs (`agg_merge_runs`): the parts' cut (a sample of
/// every run and a splitter search per part and run) costs about what it saves. CHOSEN with D3336's witness, which keeps
/// a 15,000-group case under it; no wider measurement chose it.
pub const PAR_MERGE_MIN_RECORDS: usize = 32 << 10;

/// D3336: about this many records a part (the parts' count is the records over it, within 2..=`PAR_MERGE_PARTS_MAX`):
/// enough parts for every claim thread at the floor, each a few L2-sized slices of records. CHOSEN, not measured.
pub const PAR_MERGE_PART_RECORDS: usize = 8 << 10;

/// D4620: the most grouping sets one node folds from a finer set's records (decompose ops/agg/gsets_tag.rs `cascade`:
/// each is a group, a pipeline and a breaker slot of its own), and so the most cascaded sets' regions a cascade base's
/// emit gathers after its own (kernels table/emit.rs `extra_regions`, an array of this many). A format bound with no
/// PostgreSQL constant; a node with more keeps form T's fan-out.
pub const CASCADE_SETS_MAX: usize = 32;
