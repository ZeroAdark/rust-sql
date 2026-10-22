//! Gravity's EXPLAIN ANALYZE (docs/gravity/entry.md §4): the getters the printer reads for a served statement. The
//! statement runs its plain program on the plain road (entry `start_ours`), so a node's rows print only where the run
//! counts them exactly (the root: the statement's rows) and `(actual: not counted)` elsewhere until each node's boundary
//! is counted; the run's record prints as the `Gravity` block. Every PostgreSQL detail getter answers nothing for a
//! served node (no guessed count, no time); the initial pruning getters keep PostgreSQL's answers (computed for this
//! execution's parameters). An Index Only Scan's Heap Fetches is the served walk's own count (`heap_fetches`).

use execmain_seams::TrigInstrReport;
use gravity_driver::entry::record::PipeRun;
use types_core::instrument::{AggregateInstrumentation, BitmapHeapScanInstrumentation, EngineKindWire, GravityNodeRows, GravityPipeline, GravityReport, HashInstrumentation, IncrementalSortInfo, Instrumentation, MemoizeInstrumentation, RuntimeEaPipeline, TuplesortInstrumentation, TuplestoreInstrumentation};
use types_error::PgResult;
use types_nodes::plannodes::PlannedStmt;
use types_portal::QueryDescHandle;
use types_slot::EXEC_FLAG_ENGINE_REPORT;

/// EXPLAIN ANALYZE scratch (one call's list of ids): charged to the statement's owner where the calling thread has one
/// (`gravity_driver::stmt_context`: the portal its backend executes, else its running statement), uncharged outside both.
fn scratch_owner() -> Option<memledger::Owner> {
    gravity_driver::stmt_context().map(|(o, _)| o)
}

/// The allocator refused `n` elements of scratch: the out-of-memory ERROR (53200), as a context limit raises it. Never a
/// report with a field missing.
#[cold]
#[inline(never)]
fn scratch_oom<T>(n: usize) -> ! {
    mcx::raise_oom_named("EXPLAIN ANALYZE scratch", n.saturating_mul(core::mem::size_of::<T>()))
}

/// `it` collected into scratch (`scratch_owner`); the out-of-memory ERROR when the allocator refuses.
fn scratch<T>(it: impl IntoIterator<Item = T>) -> memledger::ChargedVec<T> {
    let it = it.into_iter();
    let n = it.size_hint().0.max(1);
    memledger::ChargedVec::try_from_iter(scratch_owner(), memledger::Kind::Region, it).unwrap_or_else(|_| scratch_oom::<T>(n))
}

/// `scratch` over `Option` items: `None` at the first `None` item; the out-of-memory ERROR when the allocator refuses.
fn scratch_all<T>(it: impl IntoIterator<Item = Option<T>>) -> Option<memledger::ChargedVec<T>> {
    let mut v = memledger::ChargedVec::new(scratch_owner(), memledger::Kind::Region); // alloc: Exec charged scratch_owner()
    for x in it {
        if v.push(x?).is_err() {
            scratch_oom::<T>(v.len() + 1);
        }
    }
    Some(v)
}

use crate::admit::entry::{route, QueryDesc};

/// The root node's rows: `write` names a ModifyTable root (`Some(returning)`), `cte_write` a SELECT
/// whose program writes a data-modifying CTE (its completion count is the write's). A SELECT root returns the statement's
/// rows, a ModifyTable its RETURNING rows (one per row written), none without RETURNING; `None` = not counted (the run
/// is not done, or its count is not the root's).
pub fn root_rows(write: Option<bool>, cte_write: bool, select: bool, done: bool, rows: u64) -> Option<u64> {
    match write {
        _ if !done => None,
        Some(false) => Some(0),
        Some(true) => Some(rows),
        None if select && !cte_write => Some(rows),
        None => None,
    }
}

fn root_id(pstmt: &PlannedStmt<'_>) -> Option<i32> {
    pstmt.planTree.and_then(|n| n.as_plan()).map(|p| p.plan_node_id)
}

/// A served EXPLAIN ANALYZE's node: its rows where the run counts them exactly (the root), not counted elsewhere. `None`
/// for a desc that is no EXPLAIN ANALYZE or never served.
fn gravity_node(qd: &QueryDesc, plan_node_id: i32) -> Option<GravityNodeRows> {
    stack_depth_core::check_stack_depth_or_panic();
    let s = qd.serve.as_ref().filter(|s| s.report.is_some())?;
    if root_id(qd.plannedstmt()) != Some(plan_node_id) {
        match init_ran(qd, plan_node_id) {
            InitRan::Never => return Some(GravityNodeRows::Counted { rows: 0.0, loops: 0.0 }),
            InitRan::Unknown => return Some(GravityNodeRows::NotCounted),
            InitRan::Ran | InitRan::NoInit => {}
        }
        if let Some(leaf) = bitmap_leaf_rows(qd, plan_node_id) {
            return Some(leaf);
        }
        let rows_of = |id: i32| cell_counts(&s.a.program.instr, &s.instr, id).map(|c| c.0);
        let counted = cell_counts(&s.a.program.instr, &s.instr, plan_node_id).filter(|_| !hash_side_unknown(qd.plannedstmt(), plan_node_id, &rows_of));
        // an Index Only Scan prints its Heap Fetches with its counts: without them (no index-only site of its own) it is
        // not counted
        let counted = counted.filter(|_| !gravity_decompose::explain::is_index_only(qd.plannedstmt(), plan_node_id) || ios_fetches(qd, plan_node_id).is_some());
        let counted = counted.map(|(rows, loops)| (rows, if loops == 0 { append_loops(qd, plan_node_id).unwrap_or(0) } else { loops }));
        // D3500: a MERGE's outer plan, with the rows its rechecks handed the actions beyond the one row each rechecked
        // (driver `fold_recheck`, lowerv4 `Ctr::MergeSplit`)
        let counted = counted.map(|(rows, loops)| (rows + merge_split(qd, plan_node_id), loops));
        return Some(counted.map_or(GravityNodeRows::NotCounted, |(rows, loops)| GravityNodeRows::Counted { rows: rows as f64, loops: loops as f64 }));
    }
    let write = qd.plannedstmt().planTree.and_then(|n| n.as_modify_table()).map(|m| !m.returningLists.is_nil());
    let rows = root_rows(write, s.a.program.write.cte, s.cmd == gravity_program::Cmd::Select, s.done, s.rows);
    Some(rows.map_or(GravityNodeRows::NotCounted, |r| GravityNodeRows::Counted { rows: r as f64, loops: 1.0 }))
}

/// D3500: the rows a MERGE's rechecks handed its actions beyond the one row each rechecked, when node `id` is that MERGE's
/// outer plan (its ModifyTable's `Ctr::MergeSplit` cells, 30: a MATCHED row that no longer joins goes on as its target
/// row and its source row, which PostgreSQL's ExecMergeMatched counts on the outer plan); else 0.
fn merge_split(qd: &QueryDesc, id: i32) -> u64 {
    let Some(s) = qd.serve.as_ref() else { return 0 };
    let Some(mt) = gravity_decompose::explain::merge_outer_of(qd.plannedstmt(), id) else { return 0 };
    s.a.program.instr.iter().zip(s.instr.iter()).filter(|(c, _)| **c == (mt, 30)).map(|(_, &v)| v).sum()
}

/// Whether PostgreSQL ran the InitPlan whose plan holds a node (`init_ran`).
enum InitRan {
    /// The node is in no InitPlan's plan.
    NoInit,
    /// A node reading the InitPlan's parameters evaluated one: PostgreSQL ran it there, once, as the run did.
    Ran,
    /// None did (each never ran, or returned no row where only its output columns or its qual read one, its qual seeing no
    /// row): PostgreSQL never ran it, whatever the run did (a quiet InitPlan runs at its reader pipeline's start).
    Never,
    /// Not known from the readers' counts: its nodes are not counted.
    Unknown,
}

/// Node `id`'s InitPlan, run or not as PostgreSQL 18.6 runs it: at the first evaluation of one of its parameters
/// (ExecEvalParamExec), never before. Its readers (decompose explain `init_readers`) decide by their own counts: a reader
/// that started reads it at its start (a one-time filter, runtime index keys, a Limit's count, exec-time pruning); one that
/// returned a row read it in its qual or its output columns; one that never ran, or returned no row and reads it only in
/// its output columns (or in its qual and read no row at all: its Filter removed none), read nothing. Exact or absent:
/// any reader the counts cannot decide leaves the InitPlan's nodes not counted. `Never` prints only for an InitPlan the
/// lowering proved quiet (lowerv4 `Ctr::InitQuiet`: it calls nothing VOLATILE and cannot raise, so a run of it shows
/// nothing but its counts) or one the run itself never ran (its root counted no loop); any other is not counted.
fn init_ran(qd: &QueryDesc, id: i32) -> InitRan {
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // EXPLAIN-only scratch: the decompose walk's lists, gone with the call
    let Some((plan_id, root, readers)) = gravity_decompose::explain::init_readers(cx.mcx(), qd.plannedstmt(), id) else { return InitRan::NoInit };
    let Some(s) = qd.serve.as_ref() else { return InitRan::Unknown };
    let quiet = s.a.program.instr.iter().any(|x| x.0 == plan_id && x.1 == INIT_QUIET) || cell_counts(&s.a.program.instr, &s.instr, root).is_some_and(|c| c.1 == 0);
    let mut never = quiet;
    for (n, r) in readers {
        let (rows, loops) = match gravity_node(qd, n) {
            Some(GravityNodeRows::Counted { rows, loops }) => (rows, loops),
            _ => return InitRan::Unknown,
        };
        if loops <= 0.0 {
            continue;
        }
        if r.start || (rows > 0.0 && (r.qual || r.out)) {
            return InitRan::Ran;
        }
        let seen = || scan_filtered(qd, n).map(|f| f + rows);
        let quiet = !r.other && (!r.qual || seen() == Some(0.0));
        if r.qual_first && seen().is_some_and(|x| x > 0.0) && !r.other {
            return InitRan::Ran;
        }
        never &= quiet;
    }
    if never { InitRan::Never } else { InitRan::Unknown }
}

/// lowerv4 `Ctr::InitQuiet`: the marker cell (keyed by the InitPlan's plan id) of an InitPlan the lowering proved quiet.
const INIT_QUIET: u8 = 40;

/// lowerv4 `Ctr::SpoolRead`: the marker cell of a synthetic Materialize's node (nest.selfspool), counted by its reader alone.
const SPOOL_READ: u8 = 41;

/// A plain scan's Rows Removed by Filter where the run counted it (its node's `Ctr::Filtered1` cells, no walk or granule
/// source adding to it): the rows its Filter saw and removed. `None` otherwise.
fn scan_filtered(qd: &QueryDesc, id: i32) -> Option<f64> {
    let s = qd.serve.as_ref()?;
    let cells = &s.a.program.instr;
    if !cells.iter().any(|x| x.0 == id && x.1 == 2) || granule_scan(qd, id) || index_filtered(qd, id).is_some() || bitmap_counts(qd, id).is_some_and(|b| b.0 == id) {
        return None;
    }
    Some(cells.iter().zip(s.instr.iter()).filter(|(x, _)| x.0 == id && x.1 == 2).map(|(_, &v)| v).sum::<u64>() as f64)
}

/// An Index Scan's rows its walk's qual dropped before the program's Filter saw them (hosts `BitmapCounts::FILTERED` of
/// its site: the node is the only Index Scan over its heap and index, one plain index site walks them); `None` otherwise.
fn index_filtered(qd: &QueryDesc, id: i32) -> Option<u64> {
    let s = qd.serve.as_ref().filter(|s| s.done)?;
    let (heap, index) = gravity_decompose::explain::sole_index_scan(qd.plannedstmt(), id)?;
    let mut sites = (0..s.site_bitmap.len()).filter(|&i| s.a.registry.recipe(i).is_some_and(|r| !r.only && r.heap == heap && r.index == index));
    match (sites.next(), sites.next()) {
        (Some(i), None) => s.site_bitmap[i].map(|c| c[4]),
        _ => None,
    }
}

/// Seq Scan `id` reads a pgrcolumnar relation through granule sources (a granule site of the run is over its relation).
/// Its Rows Removed by Filter is not kept (design L2, exact or absent): the sources answer rows without a visit (zone and
/// code verdicts, the condition cache) and a bounded walk stops at a granule's vector, not at PostgreSQL's row, so the
/// count of rows its Filter removed is not the run's. EXPLAIN prints the line absent (the served instrumentation's
/// `nfiltered1` < 0, explain node.rs `show_instrumentation_count`).
fn granule_scan(qd: &QueryDesc, id: i32) -> bool {
    let Some(rel) = gravity_decompose::explain::seq_scan_rel(qd.plannedstmt(), id) else { return false };
    let Some(s) = qd.serve.as_ref() else { return false };
    (0..s.a.program.sites.len()).any(|i| s.a.registry.granule_recipe(i).is_some_and(|g| g.rel == rel))
}

/// An Append's or MergeAppend's starts where no pipeline of the run counted them (its first arm's pipeline counts them,
/// and initial pruning may have removed that arm): the most any arm ran, or with initial pruning removing every arm, the
/// starts of the node above it (PostgreSQL starts the Append with its parent, runs each arm it kept under it); `None`
/// for any other node.
fn append_loops(qd: &QueryDesc, id: i32) -> Option<u64> {
    let s = qd.serve.as_ref()?;
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // EXPLAIN-only scratch: the decompose walk's lists, gone with the call
    let (arms, parent, prune) = gravity_decompose::explain::append_family(cx.mcx(), qd.plannedstmt(), id)?;
    let loops = |n: i32| cell_counts(&s.a.program.instr, &s.instr, n).map_or(0, |c| c.1);
    let ran = arms.iter().map(|&a| loops(a)).max().unwrap_or(0);
    if ran > 0 || prune < 0 || !prune_result(qd.prune.as_deref(), prune).is_some_and(|kept| kept.is_empty()) {
        return Some(ran);
    }
    match parent? {
        p if root_id(qd.plannedstmt()) == Some(p) => Some(u64::from(s.done)),
        p => match gravity_node(qd, p)? {
            GravityNodeRows::Counted { loops, .. } => Some(loops as u64),
            GravityNodeRows::NotCounted => None,
        },
    }
}

/// A served Bitmap Index Scan under its Bitmap Heap Scan (decompose `explain::sole_bitmap_leaf`) as PostgreSQL's
/// MultiExecBitmapIndexScan counts it: the tuples its index's amgetbitmap reported over the execution (the site's
/// `BitmapCounts::NTIDS`) over the heap scan's loops (one bitmap per start); `None` where the pair or its site is not
/// one, or the heap scan is not counted.
fn bitmap_leaf_rows(qd: &QueryDesc, id: i32) -> Option<GravityNodeRows> {
    let (heap_node, counts) = bitmap_counts(qd, id)?;
    if heap_node == id {
        return None;
    }
    match gravity_node(qd, heap_node)? {
        GravityNodeRows::Counted { loops, .. } => Some(GravityNodeRows::Counted { rows: if loops > 0.0 { counts[0] as f64 } else { 0.0 }, loops }),
        GravityNodeRows::NotCounted => None,
    }
}

/// Node `id`'s bitmap pair's heap scan node and its site's counters over this execution (hosts `BitmapCounts`: tuples,
/// exact / lossy pages, rows the recheck removed, rows the walk's qual dropped), where `id` is one of the pair and one
/// site builds that index's bitmap for that heap; `None` otherwise.
fn bitmap_counts(qd: &QueryDesc, id: i32) -> Option<(i32, [u64; 5])> {
    let s = qd.serve.as_ref().filter(|s| s.done)?;
    let (heap_node, heap, index, sole) = gravity_decompose::explain::bitmap_leaf(qd.plannedstmt(), id)?;
    let ours = |i: usize| s.a.registry.bitmap_recipe(i).is_some_and(|r| r.heap == heap && r.index == index);
    let mut sites = if sole {
        scratch((0..s.site_bitmap.len()).filter(|&i| ours(i)))
    } else {
        // a pair over a heap and index another pair of the plan scans too (partition_prune #420: a write's arm and the same
        // leaf under its source's Materialize): the site the heap scan's own program pipelines drive (its `Ctr::Pipe` cells);
        // none under a Materialize over a relation the statement writes (`written_under_material`: PostgreSQL's lazy fill
        // meets the write's rows, the run's whole fill does not)
        if gravity_decompose::explain::written_under_material(qd.plannedstmt(), heap_node) {
            return None;
        }
        let p = &s.a.program.pipelines;
        scratch(node_pipes(&s.a.program.instr, heap_node).filter_map(|q| usize::try_from(p.get(q as usize)?.driving_site).ok()).filter(|&i| ours(i)))
    };
    sites.with_vec(|v| v.dedup());
    match *sites.as_slice() {
        [i] => Some((heap_node, *s.site_bitmap.get(i)?.as_ref()?)),
        _ => None,
    }
}

/// A node's rows and loops from the instrumented program's counter cells (`Program.instr`: plan node, lowerv4
/// `instr::Ctr`, 0 rows and 1 loops; `values` in that order, every participant's summed), when the program counts both.
fn cell_counts(cells: &[(i32, u8)], values: &[u64], plan_node_id: i32) -> Option<(u64, u64)> {
    let (mut rows, mut loops) = (None, None);
    for (i, (&(node, ctr), &v)) in cells.iter().zip(values).enumerate() {
        match (node == plan_node_id, ctr) {
            (true, 0) => rows = Some(rows.unwrap_or(0u64).wrapping_add(v)),
            (true, 1) => loops = Some(loops.unwrap_or(0u64).wrapping_add(v)),
            (true, 4) => return None, // the run cut its rows short of PostgreSQL's (lowerv4 `Ctr::Partial`)
            // its rows are the node's the next cell names (lowerv4 `Ctr::Same`: one counter per boundary)
            (true, 5) => rows = Some(rows.unwrap_or(0u64).wrapping_add(rows_of(cells, values, cells.get(i + 1).filter(|c| c.1 == 6)?.0)?)),
            _ => {}
        }
    }
    Some((rows?, loops?))
}

/// Node `id` is on a side of a hash join whose rows PostgreSQL may read otherwise than the run: PG's ExecHashJoin may
/// fetch the first outer row before it builds (a cost test), builds nothing after an empty outer, and reads no more of
/// the outer after an empty build (`!HJ_FILL_OUTER`), where the run builds first and reads no outer over an empty build.
/// So an outer side counts only over a build the run found non-empty or a join that keeps unmatched outer rows, and an
/// inner side only under an outer known non-empty (the join's or its outer child's rows) or a join that keeps unmatched
/// inner rows; elsewhere the side's nodes are not counted. So are the nodes decompose `explain::uncountable` names (a
/// bitmap index node, any node under a merge join). `rows_of`: a node's counted rows.
fn hash_side_unknown(pstmt: &PlannedStmt<'_>, id: i32, rows_of: &dyn Fn(i32) -> Option<u64>) -> bool {
    let positive = |n: Option<i32>| n.and_then(rows_of).is_some_and(|r| r > 0);
    if gravity_decompose::explain::uncountable(pstmt, id) {
        return true;
    }
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // EXPLAIN-only scratch: the decompose walk's lists, gone with the call
    let sides = gravity_decompose::explain::hash_sides(cx.mcx(), pstmt, id);
    let r = sides.iter().any(|h| match h.inner {
        _ if h.keeps => false,
        false => !positive(h.build),
        true => !positive(Some(h.join)) && !positive(h.outer),
    });
    r
}

/// A node's own rows (its `Rows` cells), `None` where it has none or the run cut them short.
fn rows_of(cells: &[(i32, u8)], values: &[u64], plan_node_id: i32) -> Option<u64> {
    let mut rows = None;
    for (&(node, ctr), &v) in cells.iter().zip(values) {
        match (node == plan_node_id, ctr) {
            (true, 0) => rows = Some(rows.unwrap_or(0u64).wrapping_add(v)),
            (true, 4) => return None,
            _ => {}
        }
    }
    rows
}

/// The `Gravity` block of a served EXPLAIN ANALYZE (`None` before its run is done).
fn gravity_report(qd: &QueryDesc) -> Option<GravityReport> {
    qd.serve.as_ref().filter(|s| s.done).and_then(|s| s.report.as_deref()).cloned()
}

/// The record an EXPLAIN ANALYZE's start leaves (entry `start_ours`): the admission's time and road, the program's size.
pub(crate) fn report_at_start(a: &crate::Admitted, pstmt: &'static PlannedStmt<'static>, admit_ns: u64, memo: bool) -> Box<GravityReport> {
    let (program_pipelines, sites) = (a.program.pipelines.len() as u32, a.program.sites.len() as u32);
    let programs = || core::iter::once(&a.program).chain(a.graph.iter().flat_map(|g| g.arms.iter().map(|x| &x.program).chain(g.inits.iter().map(|x| &x.program))));
    let rows_road = programs().map(|p| p.sites.iter().filter(|d| d.rows_via_am).count() as u32).sum::<u32>();
    let rels: Vec<u32> = programs().flat_map(|p| p.sites.iter().filter(|d| d.rows_via_am).map(|d| d.relation)).collect(); // alloc: Exec
    let rows_pushed = if rels.is_empty() { Vec::new() } else { crate::ops::scan::sites_heap::rows_pushed(pstmt, &rels) }; // alloc: Exec
    Box::new(GravityReport { shape: a.shape, admit_ns, memo, program_pipelines, sites, rows_road, rows_pushed, ..GravityReport::default() }) // alloc: Exec
}

fn pipelines(runs: &[PipeRun]) -> Vec<GravityPipeline> {
    runs.iter().map(|r| GravityPipeline { id: r.pipeline, arm: r.arm, width: r.width, ns: r.ns, claims: r.claims, why: r.why, site: r.site, at: r.at, buf: r.buf }).collect() // alloc: Exec
}

/// The leader's buffer counters now (pgBufferUsage, in the record's `Bufs` order): the probe an EXPLAIN ANALYZE's record
/// reads at each pipeline's start and end (driver entry/record.rs `record_open_probed`).
pub(crate) fn buffer_probe() -> gravity_driver::entry::record::Bufs {
    let u = ::instrument::pg_buffer_usage();
    [u.shared_blks_hit, u.shared_blks_read, u.shared_blks_dirtied, u.shared_blks_written, u.local_blks_hit, u.local_blks_read, u.local_blks_dirtied, u.local_blks_written, u.temp_blks_read, u.temp_blks_written]
}

fn buffer_usage(b: &[i64; 10]) -> types_core::instrument::BufferUsage {
    types_core::instrument::BufferUsage { shared_blks_hit: b[0], shared_blks_read: b[1], shared_blks_dirtied: b[2], shared_blks_written: b[3], local_blks_hit: b[4], local_blks_read: b[5], local_blks_dirtied: b[6], local_blks_written: b[7], temp_blks_read: b[8], temp_blks_written: b[9], ..Default::default() }
}

/// The program pipelines that count node `id`'s rows (lowerv4 `Ctr::Pipe` cells, each followed by its `Ctr::PipeOf`).
fn node_pipes(cells: &[(i32, u8)], id: i32) -> impl Iterator<Item = u16> + '_ {
    cells.windows(2).filter(move |w| w[0] == (id, 7) && w[1].1 == 8).map(|w| w[1].0 as u16)
}

/// A served EXPLAIN ANALYZE's node as PostgreSQL's Instrumentation, so the printer takes PostgreSQL's one road for it:
/// rows and loops where the run counts them (`gravity_node`); its time off its pipelines' runs on the run's clock (start
/// and end since the run began, inclusive of the pipelines that ran before, as a node's time includes its children's;
/// the root without a pipeline of its own: the last pipeline's start to the run's end); its buffers the leader's delta
/// over every pipeline that counts a node at or under it (the root: the statement's whole delta when kept). `None` for a
/// node not counted (printed `(actual: not counted)`) or a desc that is no served EXPLAIN ANALYZE.
fn served_instrument(qd: &QueryDesc, id: i32) -> Option<Instrumentation> {
    use types_core::instrument::{INSTRUMENT_BUFFERS, INSTRUMENT_TIMER, INSTRUMENT_WAL};
    let GravityNodeRows::Counted { rows, loops } = gravity_node(qd, id)? else { return None };
    let s = qd.serve.as_ref()?;
    let r = s.report.as_deref()?;
    let pstmt = qd.plannedstmt();
    let root = root_id(pstmt) == Some(id);
    let cells = &s.a.program.instr;
    let own = scratch(node_pipes(cells, id));
    let mut span = r.pipelines.iter().filter(|p| own.as_slice().contains(&p.id)).map(|p| (p.at, p.at.saturating_add(p.ns)));
    let (start, end) = match span.next() {
        Some(first) => span.fold(first, |a, b| (a.0.min(b.0), a.1.max(b.1))),
        None => (r.pipelines.last().map_or(0, |p| p.at), r.run_ns),
    };
    let end = if root { end.max(r.run_ns) } else { end };
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // EXPLAIN-only scratch: the decompose walk's lists, gone with the call
    let mut under = scratch(gravity_decompose::explain::subtree(cx.mcx(), pstmt, id).into_iter().flat_map(|n| node_pipes(cells, n)));
    under.with_vec(|v| {
        v.sort_unstable();
        v.dedup();
    });
    let mut buf = [0i64; 10];
    for p in r.pipelines.iter().filter(|p| under.contains(&p.id)) {
        buf.iter_mut().zip(p.buf.iter()).for_each(|(a, b)| *a += b);
    }
    let mut bufusage = buffer_usage(&buf);
    if root && s.bufusage != types_core::instrument::BufferUsage::default() {
        bufusage = s.bufusage;
    }
    let filtered = |c: u8| cells.iter().zip(s.instr.iter()).filter(|(x, _)| x.0 == id && x.1 == c).map(|(_, &v)| v).sum::<u64>() as f64;
    // D3814: a synthetic Materialize's node, whose quals ran once at its fill (lowerv4 `Ctr::SpoolRead`): its Rows Removed
    // are not PostgreSQL's per-rescan ones, so absent
    let once = cells.iter().any(|x| x.0 == id && x.1 == SPOOL_READ);
    let o = qd.instrument_options;
    Some(Instrumentation {
        need_timer: o & INSTRUMENT_TIMER != 0,
        need_bufusage: o & INSTRUMENT_BUFFERS != 0,
        need_walusage: o & INSTRUMENT_WAL != 0,
        startup: start as f64 / 1e9,
        total: end as f64 / 1e9,
        ntuples: rows,
        nloops: loops,
        // a bitmap heap scan's walk drops rows by its qual before the program's Filter (its site's count)
        // a pgrcolumnar scan's: not kept (-1, absent), see `granule_scan`
        nfiltered1: if granule_scan(qd, id) || once { -1.0 } else { filtered(2) + bitmap_counts(qd, id).filter(|b| b.0 == id).map_or(0.0, |b| b.1[4] as f64) + index_filtered(qd, id).unwrap_or(0) as f64 },
        // a bitmap heap scan's Rows Removed by Index Recheck: its site's recheck count
        // (the host's where its walk rechecks, the program's Filtered2 where a morsel form's filter does)
        nfiltered2: if once { -1.0 } else { filtered(3) + bitmap_counts(qd, id).filter(|b| b.0 == id).map_or(0.0, |b| b.1[3] as f64) },
        // an Index Only Scan's Heap Fetches, an ON CONFLICT ModifyTable's conflicts (lowerv4 `Ctr::Tuples2`)
        ntuples2: heap_fetches(qd, id).map_or_else(|| filtered(18), |h| h as f64),
        bufusage,
        ..Instrumentation::default()
    })
}

/// The served line's `pipes=` / `lead_ms=` of a recorded run (PGRUST_GRAVITY_REPORT; serial census M0): the rows and the
/// leader-only time EXPLAIN ANALYZE's block prints, through its one renderer.
pub(crate) fn served_pipes(runs: &[PipeRun], run_ns: u64) -> String {
    types_core::instrument::served_pipes(&pipelines(runs), gravity_driver::entry::record::lead_ns(runs, run_ns))
}

/// A driven run's record (complete.rs): the pipelines the driver ran, its wall time and its census (`runner::Ran`).
pub(crate) fn report_driven(r: &mut GravityReport, runs: Vec<PipeRun>, run_ns: u64, ran: &crate::runner::Ran) {
    r.road = "driver";
    r.pipelines = pipelines(&runs);
    r.lead_ns = gravity_driver::entry::record::lead_ns(&runs, run_ns);
    r.run_ns = run_ns;
    (r.width, r.sections, r.claims, r.opens, r.spilled) = (ran.width, ran.sections, ran.claims, ran.opens, ran.spill.1);
    r.granules = [ran.granules[0], ran.granules[1], ran.granules[2]];
}

/// A pulled run's record (run.rs `executor_run`, once per fetch): the emit pulled on the leader; its producers'
/// pipelines are not recorded (they run outside the driver's loop), so the wall time over every fetch.
pub(crate) fn report_pull(r: &mut GravityReport, runs: Vec<PipeRun>, run_ns: u64) {
    r.road = "pull";
    // each fetch's record starts its own clock: its pipelines start after the fetches before it, back to back
    let before = r.run_ns;
    r.pipelines.extend(pipelines(&runs).into_iter().map(|p| GravityPipeline { at: p.at + before, ..p }));
    r.lead_ns += gravity_driver::entry::record::lead_ns(&runs, run_ns);
    r.run_ns += run_ns;
}

/// The served-by block under EXPLAIN (ANALYZE, ENGINE): the root node's pipeline report carries the
/// shape as the role, the adapter's entries as claims, the sites' rows in / out.
fn pipeline(qd: &QueryDesc, plan_node_id: i32) -> Option<Vec<RuntimeEaPipeline>> {
    if qd.eflags & EXEC_FLAG_ENGINE_REPORT == 0 || qd.instrument_options == 0 {
        return None;
    }
    let s = qd.serve.as_ref()?;
    if !s.done || root_id(qd.plannedstmt()) != Some(plan_node_id) {
        return None;
    }
    let (rows_in, rows_out) = s.site_rows.iter().fold((0, 0), |a, r| (a.0 + r.0, a.1 + r.1));
    Some(vec![RuntimeEaPipeline { root_node_id: plan_node_id, member_node_id: -1, member2_node_id: -1, arm: "gravity", role: s.shape, taskset_index: 1, taskset_count: 1, workers: 1, claims: s.entries, epoch_segments: s.entries, granules: s.site_rows.len() as u64, rows_scanned: rows_in, rows_survived: rows_out, partials: s.rows, prune: [0; 7], busy_ns: 0, first_start_ns: 0, last_end_ns: 0, min_last_end_ns: 0, morsel_min_ns: 0, morsel_max_ns: 0 }]) // alloc: Cold
}

pub(crate) fn query_desc_instrument(h: QueryDescHandle, id: i32) -> Option<Instrumentation> {
    route(h, |qd| explained_instrument(qd).or_else(|| served_instrument(qd, id)))
}

/// An EXPLAIN without ANALYZE under an instrument option (BUFFERS: ExplainOnePlan's INSTRUMENT_BUFFERS): each node's
/// InstrAlloc'd Instrumentation, never started, whose zero buffer counters the printer shows (a non-text format prints
/// them). A served desc keeps none (Gravity's EXPLAIN ANALYZE reads its own record).
fn explained_instrument(qd: &mut QueryDesc) -> Option<Instrumentation> {
    use types_core::instrument::{INSTRUMENT_BUFFERS, INSTRUMENT_TIMER, INSTRUMENT_WAL};
    let o = qd.instrument_options;
    (qd.state == crate::admit::entry::State::Explained && o != 0).then(|| Instrumentation { need_timer: o & INSTRUMENT_TIMER != 0, need_bufusage: o & INSTRUMENT_BUFFERS != 0, need_walusage: o & INSTRUMENT_WAL != 0, ..Instrumentation::default() })
}

pub(crate) fn query_desc_gravity_node(h: QueryDescHandle, id: i32) -> Option<GravityNodeRows> {
    route(h, |qd| gravity_node(qd, id))
}

/// A served Index Only Scan's heap fetches (EXPLAIN's Heap Fetches, PostgreSQL's `ntuples2`; D3284): the count its
/// index-only site's walk kept over this execution (run.rs `site_fetches`), where the node's rows are counted (its walk
/// read the rows PostgreSQL's did), it is the plan's only Index Only Scan over its heap and index and the program has
/// one index-only site over them. `None` (absent) otherwise.
fn heap_fetches(qd: &QueryDesc, id: i32) -> Option<u64> {
    if !matches!(gravity_node(qd, id)?, GravityNodeRows::Counted { .. }) {
        return None;
    }
    ios_fetches(qd, id)
}

/// `heap_fetches`' count whatever the node's rows: its sole index-only site's fetches (an Index Only Scan read by any
/// other site has none, and then its node is not counted: `gravity_node`).
fn ios_fetches(qd: &QueryDesc, id: i32) -> Option<u64> {
    let s = qd.serve.as_ref()?;
    // the one index-only site exec admit paired with the node's own source (its recipe keyed by that plan node), so two
    // Index Only Scans over one index (an owner and a union arm in its nest inner) each read their own walk's count
    let mut own = (0..s.site_fetches.len()).filter(|&i| s.a.registry.recipe(i).is_some_and(|r| r.only && r.plan_node() == Some(id)));
    if let (Some(i), None) = (own.next(), own.next()) {
        return s.site_fetches[i];
    }
    let (heap, index) = gravity_decompose::explain::sole_index_only(qd.plannedstmt(), id)?;
    let mut sites = (0..s.site_fetches.len()).filter(|&i| s.a.registry.recipe(i).is_some_and(|r| r.only && r.heap == heap && r.index == index));
    match (sites.next(), sites.next()) {
        (Some(i), None) => s.site_fetches[i],
        _ => None,
    }
}

pub(crate) fn query_desc_gravity_heap_fetches(h: QueryDescHandle, id: i32) -> Option<u64> {
    route(h, |qd| heap_fetches(qd, id))
}

pub(crate) fn query_desc_gravity_report(h: QueryDescHandle) -> Option<GravityReport> {
    route(h, |qd| gravity_report(qd))
}

/// ExplainPrintTriggers' relations of a served write (D3381): the statement level's counts its finish took (the opened
/// result relation, then any trig-target one an AFTER event of the level fired on), in C's order; none for a statement
/// whose result relations fire no trigger.
pub(crate) fn query_desc_trigger_instrument(h: QueryDescHandle) -> Vec<TrigInstrReport> {
    route(h, |qd| {
        let Some(rels) = qd.triggers.as_ref() else { return Vec::new() }; // alloc: Cold
        rels.iter()
            .map(|r| TrigInstrReport {
                kind: r.kind,
                relname: r.relname.clone(), // alloc: Cold
                triggers: r.triggers.iter().map(|(name, _, con, i)| (name.clone(), *con, *i)).collect(), // alloc: Cold
            })
            .collect() // alloc: Cold
    })
}

/// `es_part_prune_results[i]` of a served desc: the subplans initial pruning kept (EXPLAIN's `Subplans Removed` and the
/// children it prints), `None` for an info without initial steps or a desc that computed none (no EXPLAIN ANALYZE).
pub fn prune_result(p: Option<&crate::ops::append::prune::Initial>, i: i32) -> Option<Vec<i32>> {
    p?.valid.get(usize::try_from(i).ok()?)?.clone() // alloc: Cold
}

/// `es_unpruned_relids` membership of a served desc; `None` (EXPLAIN takes it as unpruned) when it computed none.
pub fn rti_unpruned(p: Option<&crate::ops::append::prune::Initial>, rti: i32) -> Option<bool> {
    p.map(|p| p.unpruned.contains(&rti))
}

pub(crate) fn query_desc_prune_result(h: QueryDescHandle, i: i32) -> Option<Vec<i32>> {
    route(h, |qd| prune_result(qd.prune.as_deref(), i))
}

pub(crate) fn query_desc_rti_unpruned(h: QueryDescHandle, rti: i32) -> Option<bool> {
    route(h, |qd| rti_unpruned(qd.prune.as_deref(), rti))
}

pub(crate) fn query_desc_agg_instrument(h: QueryDescHandle, _id: i32) -> Option<AggregateInstrumentation> {
    route(h, |_| None)
}

pub(crate) fn query_desc_sort_instrument(h: QueryDescHandle, id: i32) -> Option<TuplesortInstrumentation> {
    route(h, |qd| sort_instrument(qd, id))
}

/// A counted node's child's counted rows (its lefttree's), `None` when the child is not counted.
fn child_rows(qd: &QueryDesc, id: i32) -> Option<f64> {
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // EXPLAIN-only scratch: the decompose walk's lists, gone with the call
    let child = gravity_decompose::explain::subtree(cx.mcx(), qd.plannedstmt(), id).get(1).copied()?;
    match gravity_node(qd, child)? {
        GravityNodeRows::Counted { rows, .. } => Some(rows),
        GravityNodeRows::NotCounted => None,
    }
}

/// PostgreSQL's MAXALIGN (8).
fn maxalign(n: u64) -> u64 {
    n.div_ceil(8) * 8
}

/// A type's length (`typlen`) and alignment in bytes, as the catalog has them; `None` when unknown.
fn type_layout(t: u32) -> Option<(i16, u64)> {
    let (len, _, align) = gravity_fn::typesem::expr::layout(t).ok()?;
    Some((len, match align as u8 { b'c' => 1, b's' => 2, b'i' => 4, _ => 8 }))
}

/// A minimal tuple's `t_len` for a row of `types` as heap_form_minimal_tuple lays it (no NULLs): the header
/// (SizeofMinimalTupleHeader, MAXALIGNed: 16), then each column at its alignment. A varlena or cstring column, whose
/// size only its row knows, takes its share of `width` past the fixed-width columns, short-headered (unaligned) under
/// 127 bytes. `None` when a type is unknown.
fn tuple_len(types: &[u32], width: i32) -> Option<u64> {
    let cols = scratch_all(types.iter().map(|&t| type_layout(t)))?;
    let cols = cols.as_slice();
    let fixed: u64 = cols.iter().filter(|c| c.0 > 0).map(|c| c.0 as u64).sum();
    let nvar = cols.iter().filter(|c| c.0 <= 0).count() as u64;
    let var = if nvar == 0 { 0 } else { (width.max(0) as u64).saturating_sub(fixed).max(nvar) / nvar };
    let mut off = 0u64;
    for &(len, align) in cols {
        off = match len {
            l if l > 0 => off.next_multiple_of(align) + l as u64,
            -1 if var < 127 => off + var,
            _ => off.next_multiple_of(align) + var,
        };
    }
    Some(16 + off)
}

/// GetMemoryChunkSpace of a `size`-byte palloc in an AllocSet of the default sizes: a power-of-two chunk up to the
/// 8 kB chunk limit, a block of its own past it, each with its 8-byte chunk header.
fn allocset_space(size: u64) -> u64 {
    (if size <= 8192 { size.max(8).next_power_of_two() } else { maxalign(size) }) + 8
}

/// PostgreSQL's in-memory accounting of a tuplesort or a tuplestore (USEMEM) after `n` tuples of `tuplen` bytes each under
/// `allowed` bytes (work_mem): its array (`entries` slots of `slot` bytes at first, a block of its own) grown by
/// grow_memtuples as the count reaches its size less one (doubled while no more than half the memory is used, else once
/// by the ratio left), and each tuple's charge. `None` once PostgreSQL's would go to disk (the memory spent, or the
/// array full and not grown).
fn mem_space(n: u64, tuplen: u64, allowed: u64, (entries, slot): (u64, u64)) -> Option<u64> {
    let arr = |size: u64| allocset_space(size * slot);
    let (mut size, mut used, mut grow, mut count) = (entries, arr(entries), true, 0u64);
    if used > allowed {
        return None;
    }
    loop {
        let stop = (size - 1).min(n);
        if count < stop {
            used += (stop - count) * tuplen;
            count = stop;
        }
        if used > allowed {
            return None;
        }
        if count >= n {
            return Some(used);
        }
        if grow {
            let avail = allowed - used;
            let new = if used <= avail { size * 2 } else { (size as f64 * (allowed as f64 / used as f64)) as u64 };
            grow = used <= avail;
            if new > size && avail >= (new - size) * slot {
                used = used - arr(size) + arr(new);
                size = new;
            } else {
                grow = false;
            }
        }
        used += tuplen;
        count += 1;
        if used > allowed || count >= size {
            return None;
        }
    }
}

/// tuplesort.c's SortTuple array: INITIAL_MEMTUPSIZE (1024) entries of 24 bytes.
const SORT_ARRAY: (u64, u64) = (1024, 24);
/// tuplestore.c's tuple pointer array: 16384 / sizeof(void *) (2048) entries of 8 bytes.
const STORE_ARRAY: (u64, u64) = (2048, 8);

/// A served in-memory Sort in PostgreSQL's words (Sort Method, Sort Space): a sort under a bound (`sort_bound`,
/// ExecSetTupleBound's) whose input passed twice it kept the top-N rows ('top-N heapsort', PostgreSQL's switch at
/// `bound * 2`), any other sorted all of them ('quicksort'); its space the rows it kept as tuplesort accounts them
/// (`mem_space`): one column is a Datum sort (no tuple charge), a bounded sort's tuples are AllocSet chunks, an unbounded
/// one's MAXALIGNed minimal tuples (`tuple_len`). Where the node or its input is not counted, or PostgreSQL's sort would
/// not stay in memory (its accounting, not Gravity's own spill), `None` (absent); a Sort that never ran prints none
/// (D3422: PostgreSQL's show_sort_info prints nothing before `sort_Done`).
fn sort_instrument(qd: &QueryDesc, id: i32) -> Option<TuplesortInstrumentation> {
    use types_core::instrument::{TuplesortMethod, TuplesortSpaceType};
    qd.serve.as_ref().filter(|s| s.done)?;
    if !matches!(gravity_node(qd, id)?, GravityNodeRows::Counted { loops, .. } if loops > 0.0) {
        return None;
    }
    let n = child_rows(qd, id)? as u64;
    // D3433: a Limit's count or offset that is an extern parameter (a generic plan's `LIMIT $1`) is this execution's value
    let ext = |p: i32| {
        let v = types_portal::params::slot(qd.params, p).filter(|v| !v.isnull)?;
        match v.ptype {
            types_core::INT8OID => Some(v.value.as_i64()),
            types_core::INT4OID => Some(i64::from(v.value.as_i32())),
            types_core::INT2OID => Some(i64::from(v.value.as_i16())),
            _ => None,
        }
    };
    let bound = gravity_decompose::explain::sort_bound(qd.plannedstmt(), id, &ext)?.map(|k| k.max(0) as u64);
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // EXPLAIN-only scratch: the decompose walk's lists, gone with the call
    let (types, width) = gravity_decompose::explain::input_row(cx.mcx(), qd.plannedstmt(), id)?;
    let t_len = tuple_len(&types, width)?;
    let tuplen = match (types.len(), bound) {
        (1, _) => 0,
        (_, Some(_)) => allocset_space(t_len),
        (_, None) => maxalign(t_len),
    };
    let allowed = crate::inputs::hash_mem::work_mem_bytes().max(64 << 10);
    let (method, bytes) = match bound {
        Some(k) if n > k.saturating_mul(2) => {
            // the heap took over at 2k + 1 rows: the array as it grew to there, k tuples kept
            let at = mem_space(k * 2 + 1, tuplen, allowed, SORT_ARRAY)?;
            (TuplesortMethod::TopNHeapsort, at - (k + 1) * tuplen)
        }
        _ => (TuplesortMethod::Quicksort, mem_space(n, tuplen, allowed, SORT_ARRAY)?),
    };
    Some(TuplesortInstrumentation { sortMethod: method, spaceType: TuplesortSpaceType::Memory, spaceUsed: bytes.div_ceil(1024) as i64 })
}

pub(crate) fn query_desc_incsort_instrument(h: QueryDescHandle, id: i32) -> Option<IncrementalSortInfo> {
    route(h, |qd| incsort_instrument(qd, id))
}

/// A served Incremental Sort's Full-sort and Pre-sorted Groups: its ten cells (lowerv4 `Ctr::IncFullGroups` ..
/// `Ctr::IncBad`, 19 to 28), which its top's replay of PostgreSQL's batches adds at the run's end (lowerv4
/// ops/sort/inc_replay.rs), where the node is counted, started at most once (a rescan would carry PostgreSQL's sort
/// states over), read PostgreSQL's rows, and the replay's accounting fit work_mem (no spill, no capped array growth: its
/// space is then PostgreSQL's). `None` (absent) otherwise.
fn incsort_instrument(qd: &QueryDesc, id: i32) -> Option<IncrementalSortInfo> {
    use types_core::instrument::IncrementalSortGroupInfo;
    let s = qd.serve.as_ref().filter(|s| s.report.is_some() && s.done)?;
    let cell = |c: u8| s.a.program.instr.iter().zip(s.instr.iter()).filter(|(x, _)| x.0 == id && x.1 == c).map(|(_, &v)| v).reduce(u64::wrapping_add);
    let v = scratch_all((19..=28).map(cell))?;
    let v = v.as_slice();
    let GravityNodeRows::Counted { loops, .. } = gravity_node(qd, id)? else { return None };
    let allowed = crate::inputs::hash_mem::work_mem_bytes().max(64 << 10);
    if loops > 1.0 || v[9] != 0 || v[8] > allowed {
        return None;
    }
    let g = |i: usize| IncrementalSortGroupInfo {
        groupCount: v[4 * i] as i64,
        sortMethods: v[4 * i + 1] as u32,
        totalMemorySpaceUsed: v[4 * i + 2] as i64,
        maxMemorySpaceUsed: v[4 * i + 3] as i64,
        ..IncrementalSortGroupInfo::default()
    };
    Some(IncrementalSortInfo { fullsortGroupInfo: g(0), prefixsortGroupInfo: g(1) })
}

pub(crate) fn query_desc_index_instrument(h: QueryDescHandle, _id: i32) -> Option<u64> {
    route(h, |_| None)
}

pub(crate) fn query_desc_hash_instrument(h: QueryDescHandle, id: i32) -> Option<HashInstrumentation> {
    route(h, |qd| hash_instrument(qd, id))
}

/// A served Hash in PostgreSQL's words (Buckets / Batches / Memory Usage) where PostgreSQL 18.6 plans its table in one
/// batch (ExecChooseHashTableSize over the node's estimate and width, work_mem x hash_mem_multiplier) and the run spilled
/// nothing: the original buckets PostgreSQL's sizing gives, the final ones doubled while the rows the node counted
/// outnumber them (NTUP_PER_BUCKET 1), one batch, and the rows at the plan's width as PostgreSQL's table accounts them
/// plus a pointer per bucket. `None` (absent) otherwise.
fn hash_instrument(qd: &QueryDesc, id: i32) -> Option<HashInstrumentation> {
    qd.serve.as_ref().filter(|s| s.done)?;
    let GravityNodeRows::Counted { rows, .. } = gravity_node(qd, id)? else { return None };
    let (est, width, skew) = gravity_decompose::explain::hash_estimate(qd.plannedstmt(), id)?;
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // EXPLAIN-only scratch: the decompose walk's lists, gone with the call
    let (types, _) = gravity_decompose::explain::input_row(cx.mcx(), qd.plannedstmt(), id)?;
    // ExecHashTableInsert's charge per row: HJTUPLE_OVERHEAD and the minimal tuple's t_len
    let charge = 16 + tuple_len(&types, width)?;
    Some(hash_sizes(rows as u64, est, width, skew, crate::inputs::hash_mem::hash_memory_limit() as f64, charge))
}

/// PostgreSQL's hash table sizes for a build of `n` rows (the run's count) planned at `est` rows of `width` bytes under
/// `mem` bytes of hash_mem (nodeHash.c ExecChooseHashTableSize, non-parallel: the planned buckets and batches, then
/// PostgreSQL 18's balance of batch files against table memory), then its build's growth as ExecHashTableInsert takes
/// it: in one batch the optimal buckets double while the rows outnumber them; the batches double while the batch in
/// memory and its bucket array outgrow the space allowed (ExecHashIncreaseNumBatches, which doubles the space instead
/// while the batch files' buffers would cost more; rows spread evenly over the batches); one batch's buckets end at the
/// optimal count (ExecHashIncreaseNumBuckets). The sizing plans a row at `width` (MAXALIGNed, its header's); the build
/// charges each row `charge` bytes (HJTUPLE_OVERHEAD and the row's own t_len).
fn hash_sizes(n: u64, est: f64, width: i32, skew: bool, mem: f64, charge: u64) -> HashInstrumentation {
    const PTR: u64 = 8; // sizeof(HashJoinTuple)
    const BLCKSZ: u64 = 8192;
    const MAX_ALLOC: u64 = 0x3fff_ffff;
    let tup = 16 + 16 + maxalign(width.max(0) as u64); // HJTUPLE_OVERHEAD + SizeofMinimalTupleHeader + the data
    let mut table = mem.min((usize::MAX / 2) as f64) as u64;
    let mut allowed = table;
    if skew {
        let per_mcv = tup + 8 * PTR + 4 + 16; // + SKEW_BUCKET_OVERHEAD
        let mcvs = ((table / per_mcv) * 2 / 100).min(i32::MAX as u64); // SKEW_HASH_MEM_PERCENT
        table -= mcvs * per_mcv;
    }
    let est = if est <= 0.0 { 1000.0 } else { est };
    let prev2 = |x: u64| if x == 0 { 1 } else { 1u64 << (63 - x.leading_zeros()) };
    let max_pointers = prev2((table / PTR).min(MAX_ALLOC / PTR)).min(i32::MAX as u64 / 2 + 1);
    let mut buckets = ((est.ceil().min(max_pointers as f64)) as u64).max(1024).next_power_of_two();
    let inner = est * tup as f64;
    let mut batch = 1u64;
    if inner + (PTR * buckets) as f64 > table as f64 {
        let bucket = tup + PTR;
        let sb = if table <= bucket { 1 } else { (table / bucket).next_power_of_two() };
        buckets = sb.min(max_pointers).next_power_of_two();
        let dbatch = (inner / table.saturating_sub(PTR * buckets).max(1) as f64).ceil().min(max_pointers as f64);
        batch = (dbatch as u64).max(2).next_power_of_two();
    }
    while batch > 1 && buckets <= MAX_ALLOC / PTR / 2 && allowed <= u64::MAX / 2 && batch >= allowed / BLCKSZ {
        buckets *= 2;
        allowed *= 2;
        batch /= 2;
    }
    let (first_buckets, first_batch) = (buckets, batch);
    // the build, one batch in memory at a time
    if batch == 1 {
        // the optimal buckets double as row `k` passes them; the first `k` whose rows and buckets overflow ends the
        // single batch (hash_mem is at least 64kB, past the two batch files' buffers, so the batches double)
        let can_double = |o: u64| o <= i32::MAX as u64 / 2 && o * 2 <= MAX_ALLOC / PTR;
        let (mut o, mut lo) = (buckets, 1u64);
        let overflow = loop {
            let hi = if can_double(o) { o.min(n) } else { n };
            let t = if PTR * o >= allowed { lo } else { (allowed - PTR * o) / charge + 1 };
            if t.max(lo) <= hi {
                break true;
            }
            if hi >= n {
                break false;
            }
            lo = o + 1;
            o *= 2;
        };
        buckets = o;
        if overflow {
            batch = 2;
        }
    }
    while batch > 1 && batch < (1 << 29) {
        let here = n.div_ceil(batch);
        if here * charge + PTR * buckets <= allowed {
            break;
        }
        if allowed <= batch * 2 * BLCKSZ {
            allowed *= 2;
        } else {
            batch *= 2;
        }
    }
    let here = n.div_ceil(batch);
    HashInstrumentation { nbuckets: buckets as i32, nbuckets_original: first_buckets as i32, nbatch: batch as i32, nbatch_original: first_batch as i32, space_peak: here * charge + PTR * buckets }
}

pub(crate) fn query_desc_tuplestore_instrument(h: QueryDescHandle, id: i32) -> Option<TuplestoreInstrumentation> {
    route(h, |qd| window_store(qd, id).or_else(|| cte_store(qd, id)))
}

/// A served CTE Scan's storage in PostgreSQL's words (Storage: Memory, Maximum Storage): its CTE's tuplestore, which
/// keeps every row the CTE's subplan returned (CteScan's leader appends each one it reads), in tuplestore.c's accounting
/// (`mem_space` over its pointer array; a row a Generation chunk: its MAXALIGNed t_len and an 8-byte header). `None`
/// (absent) where the CTE's subplan root is not counted or PostgreSQL's store would not stay in memory.
fn cte_store(qd: &QueryDesc, id: i32) -> Option<TuplestoreInstrumentation> {
    qd.serve.as_ref().filter(|s| s.done)?;
    // the other tuplestore node, a Materialize (`material_store`), in the same accounting
    if gravity_decompose::explain::is_material(qd.plannedstmt(), id) {
        return material_store(qd, id);
    }
    if !matches!(gravity_node(qd, id)?, GravityNodeRows::Counted { .. }) {
        return None;
    }
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // EXPLAIN-only scratch: the decompose walk's lists, gone with the call
    let (root, types, width) = gravity_decompose::explain::cte_source(cx.mcx(), qd.plannedstmt(), id)?;
    let GravityNodeRows::Counted { rows, .. } = gravity_node(qd, root)? else { return None };
    let chunk = maxalign(tuple_len(&types, width)?) + 8;
    let allowed = crate::inputs::hash_mem::work_mem_bytes();
    let most = mem_space(rows as u64, chunk, allowed, STORE_ARRAY)?;
    Some(TuplestoreInstrumentation { space_type: types_core::instrument::TuplesortSpaceType::Memory, max_space: most as i64 })
}

/// A served Materialize's store in PostgreSQL's words (Storage: Memory, Maximum Storage): ExecMaterial's tuplestore keeps
/// every row its child returned (the child's counted rows; a nest's inner Material is rescanned, so it has one), in
/// tuplestore.c's accounting (`mem_space` over its pointer array; a row a Generation chunk: its MAXALIGNed t_len and an
/// 8-byte header). `None` (absent) where the node never ran, it or its child is not counted, or
/// PostgreSQL's store would not stay in memory.
fn material_store(qd: &QueryDesc, id: i32) -> Option<TuplestoreInstrumentation> {
    qd.serve.as_ref().filter(|s| s.done)?;
    let GravityNodeRows::Counted { loops, .. } = gravity_node(qd, id)? else { return None };
    if loops <= 0.0 {
        return None;
    }
    let n = child_rows(qd, id)? as u64;
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // EXPLAIN-only scratch: the decompose walk's lists, gone with the call
    let (types, width) = gravity_decompose::explain::input_row(cx.mcx(), qd.plannedstmt(), id)?;
    let chunk = maxalign(tuple_len(&types, width)?) + 8;
    let allowed = crate::inputs::hash_mem::work_mem_bytes();
    let most = mem_space(n, chunk, allowed, STORE_ARRAY)?;
    Some(TuplestoreInstrumentation { space_type: types_core::instrument::TuplesortSpaceType::Memory, max_space: most as i64 })
}

/// A served WindowAgg's partition buffer in PostgreSQL's words (Storage: Memory, Maximum Storage), tuplestore.c's
/// accounting of the rows it held at once (`mem_space` over its pointer array; a row a Generation chunk: its MAXALIGNed
/// t_len and an 8-byte header). Without PARTITION BY its one partition is every input row; a frame from the partition's
/// first row keeps them all, any other trims the rows behind it, so its space is printed only where one row and every row
/// come to the same kB. `None` (absent) with PARTITION BY, for any other node, where the node or its input is not
/// counted, or PostgreSQL's buffer would not stay in memory.
fn window_store(qd: &QueryDesc, id: i32) -> Option<TuplestoreInstrumentation> {
    qd.serve.as_ref().filter(|s| s.done)?;
    let (partitioned, whole) = gravity_decompose::explain::window_frame(qd.plannedstmt(), id)?;
    if partitioned || !matches!(gravity_node(qd, id)?, GravityNodeRows::Counted { .. }) {
        return None;
    }
    let n = child_rows(qd, id)? as u64;
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // EXPLAIN-only scratch: the decompose walk's lists, gone with the call
    let (types, width) = gravity_decompose::explain::input_row(cx.mcx(), qd.plannedstmt(), id)?;
    let chunk = maxalign(tuple_len(&types, width)?) + 8;
    let allowed = crate::inputs::hash_mem::work_mem_bytes();
    let most = mem_space(n, chunk, allowed, STORE_ARRAY)?;
    let least = if whole { most } else { mem_space(n.min(1), chunk, allowed, STORE_ARRAY)? };
    (least.div_ceil(1024) == most.div_ceil(1024)).then(|| TuplestoreInstrumentation { space_type: types_core::instrument::TuplesortSpaceType::Memory, max_space: most as i64 })
}

pub(crate) fn query_desc_memoize_instrument(h: QueryDescHandle, id: i32) -> Option<MemoizeInstrumentation> {
    route(h, |qd| memoize_instrument(qd, id))
}

/// A served Memoize's cache counters: its five cells (lowerv4 `Ctr::Hits` .. `Ctr::MemPeak`, 9 to 13), which the run's
/// end writes from its memo object (exec `State::fill_cells`); `None` without them.
fn memoize_instrument(qd: &QueryDesc, id: i32) -> Option<MemoizeInstrumentation> {
    let s = qd.serve.as_ref().filter(|s| s.report.is_some() && s.done)?;
    let at = s.a.program.instr.iter().position(|&(n, c)| n == id && c == 9)?;
    let v = s.instr.get(at..at + 5)?;
    Some(MemoizeInstrumentation { cache_hits: v[0], cache_misses: v[1], cache_evictions: v[2], cache_overflows: v[3], mem_peak: v[4] })
}

pub(crate) fn query_desc_bitmap_instrument(h: QueryDescHandle, id: i32) -> Option<BitmapHeapScanInstrumentation> {
    route(h, |qd| bitmap_instrument(qd, id))
}

/// A served Bitmap Heap Scan's Heap Blocks (exact / lossy): the pages its site's walk read by the kind of their bitmap
/// entry, where the heap scan is counted and its bitmap is one Bitmap Index Scan's; `None` (absent) otherwise.
fn bitmap_instrument(qd: &QueryDesc, id: i32) -> Option<BitmapHeapScanInstrumentation> {
    let (heap_node, c) = bitmap_counts(qd, id).filter(|b| b.0 == id)?;
    matches!(gravity_node(qd, heap_node)?, GravityNodeRows::Counted { .. }).then_some(BitmapHeapScanInstrumentation { exact_pages: c[1], lossy_pages: c[2] })
}

pub(crate) fn query_desc_index_searches(h: QueryDescHandle, id: i32) -> Option<u64> {
    route(h, |qd| index_searches(qd, id))
}

/// A served index node's searches (PostgreSQL 18's Index Searches): one descent per start of the node, its counted loops,
/// where the node's index quals hold no array and give PostgreSQL 18's btree no skip array (every index column before the
/// last one a qual keys has an equality or IS NULL key: a column without one is a skip array, whose primitive scans follow
/// the btree's leaf layout); `None` (absent) otherwise (a ScalarArrayOp descends once per element group, which the run does
/// not count either).
fn index_searches(qd: &QueryDesc, id: i32) -> Option<u64> {
    let GravityNodeRows::Counted { loops, .. } = gravity_node(qd, id)? else { return None };
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // EXPLAIN-only scratch: the decompose walk's lists, gone with the call
    let keys = gravity_decompose::explain::index_keys(cx.mcx(), qd.plannedstmt(), id)?;
    let last = keys.iter().map(|k| k.0).max().unwrap_or(0);
    let equal = |col: i16| keys.iter().any(|&(c, op)| c == col && op.map_or(true, |op| gravity_fn::typesem::expr::index_equality(op).unwrap_or(false)));
    (1..last).all(equal).then_some(loops as u64)
}

pub(crate) fn query_desc_jit_instr(h: QueryDescHandle) -> (i32, i32, u64) {
    route(h, |_| (0, 0, 0))
}

pub(crate) fn query_desc_runtime_ea_refusals(h: QueryDescHandle, _id: i32) -> Option<Vec<(&'static str, &'static str)>> {
    route(h, |_| None)
}

pub(crate) fn query_desc_runtime_ea_pipeline(h: QueryDescHandle, id: i32) -> Option<Vec<RuntimeEaPipeline>> {
    route(h, |qd| pipeline(qd, id))
}

pub(crate) fn query_desc_engine_events(h: QueryDescHandle, _id: i32) -> Option<Vec<(EngineKindWire, &'static str, &'static str)>> {
    route(h, |_| None)
}

pub(crate) fn query_desc_workers_launched(h: QueryDescHandle, id: i32) -> Option<i32> {
    route(h, |qd| workers_launched(qd, id))
}

/// A served Gather's or Gather Merge's workers: the widest section among the pipelines that count a node under it (the
/// pool's participants that ran its claims), 0 when they all ran on the leader; `None` before the run is done.
fn workers_launched(qd: &QueryDesc, id: i32) -> Option<i32> {
    let s = qd.serve.as_ref().filter(|s| s.done)?;
    let r = s.report.as_deref()?;
    let cells = &s.a.program.instr;
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // EXPLAIN-only scratch: the decompose walk's lists, gone with the call
    let under = scratch(gravity_decompose::explain::subtree(cx.mcx(), qd.plannedstmt(), id).into_iter().flat_map(|n| node_pipes(cells, n)));
    Some(r.pipelines.iter().filter(|p| under.as_slice().contains(&p.id) && p.arm == "section").map(|p| p.width as i32).max().unwrap_or(0))
}

pub(crate) fn query_desc_merge_instrument(h: QueryDescHandle, id: i32) -> Option<(f64, f64, f64)> {
    route(h, |qd| merge_instrument(qd, id))
}

/// A served MERGE's rows its actions inserted, updated and deleted (PostgreSQL's mt_merge_inserted / _updated /
/// _deleted, EXPLAIN ANALYZE's `Tuples:` line): its ModifyTable node's cells (lowerv4 `Ctr::MergeInserted` ..
/// `Ctr::MergeDeleted`, 15 to 17; a kind of action the statement lacks has none and wrote nothing), `None` where the
/// program counts none.
fn merge_instrument(qd: &QueryDesc, id: i32) -> Option<(f64, f64, f64)> {
    let s = qd.serve.as_ref().filter(|s| s.done && s.report.is_some())?;
    let (mut any, mut v) = (false, [0u64; 3]);
    for (&(node, ctr), &x) in s.a.program.instr.iter().zip(s.instr.iter()) {
        if node == id && (15..=17).contains(&ctr) {
            any = true;
            v[(ctr - 15) as usize] += x;
        }
    }
    any.then_some((v[0] as f64, v[1] as f64, v[2] as f64))
}

pub(crate) fn query_desc_worker_instrument(h: QueryDescHandle, _id: i32) -> Option<Vec<Instrumentation>> {
    route(h, |_| None)
}

/// The per-worker HashAgg instrumentation (EXPLAIN ANALYZE's Worker N: Batches / Memory Usage): an entry statement has no workers.
pub(crate) fn query_desc_worker_agg_instrument(h: QueryDescHandle, _id: i32) -> Option<Vec<(i32, AggregateInstrumentation)>> {
    route(h, |_| None)
}

/// The FDW ModifyTable batch size (EXPLAIN ANALYZE's Batch Size): an entry statement runs no foreign modify.
pub(crate) fn query_desc_fdw_batch_size(h: QueryDescHandle, _rti: u32) -> Option<i32> {
    route(h, |_| None)
}

pub(crate) fn query_desc_worker_sort_instrument(h: QueryDescHandle, _id: i32) -> Option<Vec<(i32, TuplesortInstrumentation)>> {
    route(h, |_| None)
}

pub(crate) fn query_desc_worker_bitmap_instrument(h: QueryDescHandle, _id: i32) -> Option<Vec<(i32, BitmapHeapScanInstrumentation)>> {
    route(h, |_| None)
}

pub(crate) fn query_desc_worker_memoize_instrument(h: QueryDescHandle, _id: i32) -> Option<Vec<(i32, MemoizeInstrumentation)>> {
    route(h, |_| None)
}

pub(crate) fn query_desc_worker_incsort_instrument(h: QueryDescHandle, _id: i32) -> Option<Vec<(i32, IncrementalSortInfo)>> {
    route(h, |_| None)
}

pub(crate) fn query_desc_foreign_explain(h: QueryDescHandle, _id: i32, _flags: types_nodes::FdwExplainFlags, _emit: &mut dyn FnMut(&str, types_nodes::FdwExplainProp<'_>) -> PgResult<()>) -> PgResult<()> {
    route(h, |_| Ok(()))
}

pub(crate) fn cursor_capture_current(h: QueryDescHandle) -> PgResult<Option<(types_core::Oid, u64)>> {
    route(h, |_| Ok(None))
}
