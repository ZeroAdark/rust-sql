//! Tier 0 (H16 (d), main 09-24): the knobs of the interpreted first execution. The backend reads both at each code-cache
//! miss (`gravity_backend::TIER0_GUC` / `TIER0_ENV`, no per-statement read) against the rows the statement reads (`rows_read`,
//! handed over around the admission); the driver's per-execution gate is `Entry::start`'s.

use mcx::Mcx;
use types_nodes::{Node, PlannedStmt};
use types_storage::lock::NoLock;

use super::{Kind, Knob};

pub static KNOBS: &[Knob] = &[
    Knob {
        name: "pgrust.gravity_tier0_rows",
        kind: Kind::Custom,
        default: "derived (the cost home's break-even for the program's IR: gravity_program::cost::compile)",
        doc: "a code-cache miss whose pipelines are serial and each predict at most this many rows runs its first execution interpreted and compiles at its second; 0 = off (every miss compiles at once, as before); unset = the rows whose interpretation costs half the compile of the program's IR (a projection's lines counted on the rows its plan node emits)",
    },
    Knob { name: "PGRUST_GRAVITY_TIER0", kind: Kind::Env, default: "unset", doc: "force: every miss the interpreter can take is tier 0 whatever its rows (the agreement run); off: tier 0 off whatever the option says; fail: force, and every promotion's compile fails (fault injection: the code stays interpreted) (read once)" },
    Knob { name: "PGRUST_GRAVITY_TIER0_HOLD_MS", kind: Kind::Env, default: "unset (no hold)", doc: "fault injection (D3811): the compile pool's compile of a module a tier-0 miss hands it holds the module's key this many ms (at most 60000, the cost home's TIER0_HOLD_MAX_MS) before it compiles, so the program's next execution waits for it as for a long compile; server start only (read once)" },
];

/// The rows a statement reads, tier 0's gate (`gravity_backend::with_row_estimate`, main 09-24): over every plan node (the
/// subplans' included) the most rows it is planned to emit, and for a scan with a residual filter the most it can read, its
/// relation's tuples now (`Walk::scan_rows`; `plan_rows` counts after the filter: an index range reading 300k rows to keep
/// 10 plans 10), except an
/// index probe the facts home bounds (`rows::probe_rows`: a full-key unique point lookup reads one row at most, the filter
/// keeps what `plan_rows` says). A relation that does not open reads `u64::MAX` (compiled at once).
///
/// D3867: a heap relation no ANALYZE has counted (never analyzed, or analyzed empty and grown since) reads its line
/// pointers while it holds at most `UNCOUNTED_BLOCKS` blocks (`heap_source::small_line_pointers`), and `u64::MAX` past
/// that. Its count is a bound, not an estimate (every tuple a scan returns has a line pointer), so it also holds the planned
/// rows of the nodes above it (`Walk::rows`): a scan (and its bitmap) to its relation's count, a join to its sides' bounds
/// (`join_rows`), an Append to its children's sum, a node that never emits more rows than it takes in to its input's bound
/// (`passes_rows`). PostgreSQL plans such a relation at
/// 10 blocks of tuples at least (`table_block_relation_estimate_size`), so an ORM's freshly made tables (the Rails and Django
/// suites never ANALYZE theirs) planned thousands of rows each and every new statement over them compiled at once: 1-5 ms of
/// compile on the statement's path, where interpreting its first execution over a few rows costs microseconds.
///
/// Answers (the rows read, the most any plan node may emit: `u64::MAX` unless every node is bounded), the second the
/// driver's gate holds each pipeline's prediction to (`gravity_backend::with_row_estimate`).
pub(crate) fn rows_read(pstmt: &PlannedStmt<'_>, mcx: Mcx<'_>) -> (u64, u64) {
    let w = Walk { pstmt, mcx };
    let top = w.rows(pstmt.planTree, u64::MAX);
    pstmt.subplans.iter().map(|s| w.rows(s, u64::MAX)).fold((top.most, top.all), |(m, a), r| (m.max(r.most), a.max(r.all)))
}

/// D4005: each tagged plan node's lines (`gravity_lower::Lowered::tagged`: a projection's entries) with the rows that run
/// them, the node's planned rows (`plan_rows`, per execution of the node), for tier 0's break-even (gravity_backend
/// `mapped_cached_keyed`: a tagged line weighs its node's rows, at most the statement's). A node the statement may run more
/// than once (under a nested loop's or a recursive union's inner side, or in a subplan's tree) answers `u64::MAX`, and one
/// not found answers nothing: their lines weigh the statement's rows, as before.
pub(crate) fn site_rows(pstmt: &PlannedStmt<'_>, tagged: &[(i32, u32)]) -> Vec<(u32, u64)> {
    fn once(n: Node<'_>, again: bool, tagged: &[(i32, u32)], out: &mut Vec<(u32, u64)>) {
        gravity_decompose::walk::stack_check();
        let Some(p) = n.as_plan() else { return };
        if let Some(&(_, lines)) = tagged.iter().find(|t| t.0 == p.plan_node_id) {
            let planned = if p.plan_rows.is_finite() && p.plan_rows >= 0.0 { p.plan_rows.ceil() as u64 } else { u64::MAX };
            out.push((lines, if again { u64::MAX } else { planned })); // alloc: Cold
        }
        let rescans = n.as_nest_loop().is_some() || n.as_recursive_union().is_some();
        if let Some(l) = p.lefttree {
            once(l, again, tagged, out);
        }
        if let Some(r) = p.righttree {
            once(r, again || rescans, tagged, out);
        }
        let lists = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|a| &a.mergeplans)];
        for c in lists.into_iter().flatten().flat_map(|l| l.iter()) {
            once(c, again, tagged, out);
        }
        if let Some(s) = n.as_subquery_scan().and_then(|s| s.subplan) {
            once(s, again, tagged, out);
        }
    }
    let mut out = Vec::with_capacity(tagged.len()); // alloc: Cold
    if tagged.is_empty() {
        return out;
    }
    if let Some(root) = pstmt.planTree {
        once(root, false, tagged, &mut out);
    }
    for r in pstmt.subplans.iter().flatten() {
        once(r, true, tagged, &mut out);
    }
    out
}

/// One plan subtree's rows (`Walk::rows`).
#[derive(Clone, Copy)]
struct Rows {
    /// The most rows a node of the subtree is planned to emit, or reads (a scan with a residual filter).
    most: u64,
    /// A bound on the rows the subtree's root emits (`u64::MAX`: none).
    emits: u64,
    /// The largest of the subtree's nodes' `emits`.
    all: u64,
}

/// D3867: the most blocks an uncounted relation may hold for tier 0's gate to count its line pointers (each block one
/// buffer read, pinned and share-locked, at a lowered admission). Past it the relation reads `u64::MAX`, as before: the
/// count costs a read per block, and 8 blocks hold up to 2,328 heap tuples, past the largest cap the cost home gives (about
/// 1,100 rows, at 810 IR lines).
pub(crate) const UNCOUNTED_BLOCKS: u64 = 8;

const HEAP_TABLE_AM_OID: types_core::Oid = 2;

struct Walk<'a, 'p, 'm> {
    pstmt: &'a PlannedStmt<'p>,
    mcx: Mcx<'m>,
}

impl Walk<'_, '_, '_> {
    /// The rows of the tree under `root` (`Rows`). A node's planned rows are held to its bound (D3867); with no uncounted
    /// relation under it every bound is `u64::MAX` and `most` is the planned rows alone, as before. `tids`: the bound on
    /// the TIDs a bitmap subtree yields, its heap scan's (an index entry names one heap tuple's line pointer).
    fn rows(&self, root: Option<Node<'_>>, tids: u64) -> Rows {
        stack_depth_core::check_stack_depth_or_panic();
        let Some(n) = root else { return Rows { most: 0, emits: 0, all: 0 } };
        let Some(p) = n.as_plan() else { return Rows { most: 0, emits: u64::MAX, all: u64::MAX } };
        if unbounded_srf(n) {
            return Rows { most: u64::MAX, emits: u64::MAX, all: u64::MAX };
        }
        let planned = if p.plan_rows.is_finite() && p.plan_rows > 0.0 { p.plan_rows as u64 } else { 0 };
        let scan = scan_of(n).map(|s| self.scan_rows(s.scanrelid));
        let bitmap = n.as_bitmap_index_scan().is_some() || n.as_bitmap_and().is_some() || n.as_bitmap_or().is_some();
        let under = match (scan, bitmap) {
            (Some((_, b)), _) => b,
            (None, true) => tids,
            (None, false) => u64::MAX,
        };
        let (l, r) = (self.rows(p.lefttree, under), self.rows(p.righttree, under));
        let (mut most, mut all) = (l.most.max(r.most), l.all.max(r.all));
        let mut emits = match (scan, join_of(n)) {
            (Some((_, b)), _) => b,
            (None, _) if bitmap => tids,
            (None, Some(j)) => join_rows(j, l.emits, r.emits),
            (None, None) if p.lefttree.is_some() && p.righttree.is_none() && passes_rows(n) => l.emits.max(1),
            // a Result over no input: one row (none when its constant qual fails)
            (None, None) if p.lefttree.is_none() && p.righttree.is_none() && n.as_result().is_some() => 1,
            (None, None) => u64::MAX,
        };
        let mut kids = |c: Rows| {
            most = most.max(c.most);
            all = all.max(c.all);
            c.emits
        };
        if let Some(list) = n.as_append().map(|a| &a.appendplans).or_else(|| n.as_merge_append().map(|a| &a.mergeplans)) {
            emits = list.iter().fold(0u64, |e, c| e.saturating_add(kids(self.rows(Some(c), u64::MAX))));
        }
        if let Some(list) = n.as_bitmap_and().map(|a| &a.bitmapplans).or_else(|| n.as_bitmap_or().map(|o| &o.bitmapplans)) {
            for c in list.iter() {
                kids(self.rows(Some(c), tids));
            }
        }
        if let Some(s) = n.as_subquery_scan() {
            emits = kids(self.rows(s.subplan, u64::MAX));
        }
        most = most.max(planned.min(emits));
        if let (Some((tuples, _)), false) = (scan, p.qual.is_nil()) {
            let facts = gravity_hosts::facts::RelcacheFacts::new(self.mcx);
            most = most.max(gravity_decompose::catalog::rows::probe_rows(&facts, n).unwrap_or(tuples));
        }
        Rows { most, emits, all: all.max(emits) }
    }

    /// The scanned relation's tuples now, and an upper bound on the rows a scan of it returns (`u64::MAX` = none).
    /// The tuples: `reltuples` while it holds no more blocks than `relpages`, else its catalog density over the blocks it
    /// holds now (one smgr size read), as PostgreSQL's own size estimate scales a relation grown since its last ANALYZE (a
    /// table analyzed at 600 rows and loaded to 300,000 since reads 300,000, not 600). D3867: a heap relation no ANALYZE has
    /// counted (never analyzed, or grown from no pages) reads its line pointers while it holds at most `UNCOUNTED_BLOCKS`
    /// blocks, which is also the bound; `u64::MAX` past that, or when it does not open.
    fn scan_rows(&self, scanrelid: u32) -> (u64, u64) {
        let Some(relid) = (scanrelid as usize).checked_sub(1).filter(|&i| i < self.pstmt.rtable.len()).and_then(|i| self.pstmt.rtable.nth(i).as_range_tbl_entry()).map(|r| r.relid) else { return (u64::MAX, u64::MAX) };
        let Ok(r) = table::table_open(self.mcx, relid, NoLock) else { return (u64::MAX, u64::MAX) };
        // C3 S5 (D3889): size statistics (a TRUNCATE's storage-scoped message moves them, not the entry)
        gravity_deps::note_stats(relid);
        let (t, pages, heap) = (r.rd_rel.reltuples, r.rd_rel.relpages, r.rd_rel.relam == HEAP_TABLE_AM_OID);
        let _ = r.close(NoLock);
        if t.is_finite() && t >= 0.0 {
            let Ok(now) = gravity_hosts::ops::scan::heap_source::nblocks(self.mcx, relid) else { return (u64::MAX, u64::MAX) };
            let grown = grown_tuples(t as f64, pages.max(0) as u64, now);
            if grown != u64::MAX {
                return (grown, u64::MAX);
            }
        }
        if !heap {
            return (u64::MAX, u64::MAX);
        }
        match gravity_hosts::ops::scan::heap_source::small_line_pointers(self.mcx, relid, UNCOUNTED_BLOCKS) {
            Ok(Some(lines)) => (lines, lines),
            _ => (u64::MAX, u64::MAX),
        }
    }
}

/// The join node's `Join` (its type and the planner's uniqueness proof).
fn join_of<'n>(n: Node<'n>) -> Option<&'n types_nodes::plannodes::Join<'n>> {
    n.as_nest_loop().map(|j| &j.join).or_else(|| n.as_hash_join().map(|j| &j.join)).or_else(|| n.as_merge_join().map(|j| &j.join))
}

/// D3867: a bound on the rows a join of sides bounded by `l` (outer) and `r` (inner) rows emits: each outer row once per
/// inner match or null-extended, and each inner row once more unmatched (a right / full join): `l * r + l + r`. An inner
/// side the planner proved unique on the join clauses (`inner_unique`) matches each outer row at most once, and a semi /
/// anti join emits each of its side's rows at most once: `l + r`.
fn join_rows(j: &types_nodes::plannodes::Join<'_>, l: u64, r: u64) -> u64 {
    use types_nodes::jointype::JoinType::{JOIN_ANTI, JOIN_RIGHT_ANTI, JOIN_RIGHT_SEMI, JOIN_SEMI};
    let sides = l.saturating_add(r);
    match j.inner_unique || matches!(j.jointype, JOIN_SEMI | JOIN_ANTI | JOIN_RIGHT_SEMI | JOIN_RIGHT_ANTI) {
        true => sides,
        false => l.saturating_mul(r).saturating_add(sides),
    }
}

/// D3867: a plan node that never emits more rows than its one input brings (`max(input, 1)`: a plain aggregate's one row
/// over none): a sort, a hash, a materialize / memoize, a de-duplication, a limit, a row lock, a gather, a window, a
/// group, a set operation, a projecting Result, a ModifyTable's RETURNING, an aggregate without grouping sets.
fn passes_rows(n: Node<'_>) -> bool {
    n.as_sort().is_some()
        || n.as_incremental_sort().is_some()
        || n.as_hash().is_some()
        || n.as_material().is_some()
        || n.as_memoize().is_some()
        || n.as_unique().is_some()
        || n.as_limit().is_some()
        || n.as_lock_rows().is_some()
        || n.as_gather().is_some()
        || n.as_gather_merge().is_some()
        || n.as_window_agg().is_some()
        || n.as_group().is_some()
        || n.as_set_op().is_some()
        || n.as_result().is_some()
        || n.as_modify_table().is_some()
        || n.as_agg().is_some_and(|a| a.groupingSets.is_nil())
}

/// D1227 / D1228: a ProjectSet or a FunctionScan calling a set-returning function whose rows the planner could not know:
/// no planner support function (`prosupport`, so `plan_rows` is the catalog's `prorows` guess, 100 for
/// jsonb_array_elements / json_each) over an argument that is not a constant (a column's document: 200 k elements read as
/// 100). Its rows are unknown, so the gate compiles at once: tier 0 interpreted the whole set on the first execution (the
/// cold N2 / N3 cells, ~60 ms of interpretation against ~0.5 ms of compile).
fn unbounded_srf(n: Node<'_>) -> bool {
    let mut exprs: Vec<Node<'_>> = Vec::new(); // alloc: Cold
    if let Some(ps) = n.as_project_set() {
        exprs.extend(ps.plan.targetlist.iter());
    } else if let Some(fs) = n.as_function_scan() {
        exprs.extend(fs.functions.iter().filter_map(|f| f.as_range_tbl_function().and_then(|r| r.funcexpr)));
    } else {
        return false;
    }
    let mut unbounded = false;
    for e in exprs {
        gravity_decompose::expr::walk(e, &mut |x| {
            if let Some(f) = x.as_func_expr().filter(|f| f.funcretset) {
                let guessed = lsyscache::get_func_support(f.funcid).map_or(true, |s| s == 0);
                unbounded |= guessed && f.args.iter().any(|a| strip(a).as_const().is_none());
            }
            !unbounded
        });
    }
    unbounded
}

/// `reltuples` (`tuples` over `pages` blocks at the last ANALYZE) of a relation holding `now` blocks: the same while it has
/// not grown, its density over `now` blocks once it has, `u64::MAX` when it grew from no pages (no density to scale).
fn grown_tuples(tuples: f64, pages: u64, now: u64) -> u64 {
    if now <= pages {
        tuples as u64
    } else if pages == 0 {
        u64::MAX
    } else {
        (tuples / pages as f64 * now as f64).ceil() as u64
    }
}

fn strip(n: Node<'_>) -> Node<'_> {
    n.as_relabel_type().map_or(n, |r| r.arg)
}

/// The scan node's `Scan` (its relation): the heap / index / bitmap / tid scans.
fn scan_of<'n>(n: Node<'n>) -> Option<&'n types_nodes::plannodes::Scan<'n>> {
    n.as_seq_scan().map(|s| &s.scan).or_else(|| n.as_index_scan().map(|s| &s.scan)).or_else(|| n.as_index_only_scan().map(|s| &s.scan)).or_else(|| n.as_bitmap_heap_scan().map(|s| &s.scan)).or_else(|| n.as_tid_range_scan().map(|s| &s.scan)).or_else(|| n.as_tid_scan().map(|s| &s.scan))
}

#[cfg(test)]
mod tests {
    /// The census rows name what the backend reads.
    #[test]
    fn the_rows_are_the_backends_names() {
        assert_eq!(super::KNOBS[0].name, gravity_backend::TIER0_GUC);
        assert_eq!(super::KNOBS[1].name, gravity_backend::TIER0_ENV);
        assert_eq!(super::KNOBS[2].name, gravity_backend::TIER0_HOLD_ENV);
    }

    /// A relation grown since its ANALYZE reads its catalog density over the blocks it holds now, never its stale count.
    #[test]
    fn a_grown_relation_reads_its_density_over_its_blocks_now() {
        use super::grown_tuples;
        assert_eq!(grown_tuples(600.0, 5, 5), 600, "not grown: the catalog's count");
        assert_eq!(grown_tuples(600.0, 5, 3), 600, "shrunk (truncated pages not yet counted): the catalog's count");
        assert_eq!(grown_tuples(600.0, 5, 2_500), 300_000, "analyzed at 600 rows, 2,500 blocks now");
        assert_eq!(grown_tuples(0.0, 0, 0), 0, "analyzed empty, still empty");
        assert_eq!(grown_tuples(0.0, 0, 40), u64::MAX, "analyzed empty, grown since: unknown");
    }
}
