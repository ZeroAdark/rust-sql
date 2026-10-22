//! M10's runtime protocol for v4 programs (window.agg): `Window`, one WindowAgg's window
//! object (hosts v4/m10.rs `WinObj`) per execution, its handle laid into the proto's word before any pipeline runs. The
//! object's layout is decompose window.rs's: the child's columns, then each window function's argument and FILTER cells
//! (`arg_cells`) in ExecInitWindowAgg's order (`window_funcs`). The object and its stored rows drop with the execution
//! on every path. Created by M10-WINDOW.
use core::cell::Cell;
use std::rc::Rc;

use gravity_decompose::catalog::expr_type;
use gravity_hosts::ops::window::object::{WinObj, WinSpec};
use gravity_parallel::{ClaimSize, MorselSource, PartitionRange, RangeCursor};
use gravity_program::protos::{Proto, ProtoKind};
use memledger::ChargedVec;
use types_core::Oid;
use types_error::PgResult;
use types_nodes::plannodes::{PlannedStmt, WindowAgg};
use types_nodes::Node;

use crate::ops::{Live, Prepare, Protocol};
use crate::admit::{Admit, Bind};
use crate::glue;

pub const ADMIT: &[Admit] = &[];
pub const BIND: &[Bind] = &[];
pub const PROTOCOLS: &[&dyn Protocol] = &[&Window];

pub struct Window;

/// The execution's window object (D3336: or a parted window's objects, each a claim of its reader, and their table).
struct WinLive {
    _obj: Option<Box<WinObj>>,
    parts: Option<Parts>,
    /// The exact request (W4): the pulled fill's set and the fill's readers.
    pulled: Option<(Rc<Pull>, usize, memledger::ChargedVec<usize>)>, // alloc: Exec
}

/// A pulled fill (the exact request, PLAN-v5 §2.5; W4): its set in program order (the fill last), deferred (the producer
/// order steps past it), run only by the object's spool through the runner's frame (`raw::pull`), as nest.cte's pull.
struct Pull {
    members: ChargedVec<u16>, // alloc: Exec
    frame: Cell<Option<crate::ops::Frame>>,
    running: Cell<bool>,
    done: Cell<bool>,
    failed: Cell<bool>,
}

/// D3336 (win.partruns): a parted window's objects, their table (the handle word holds its address) and its reader's
/// claims, one object each (`object_claims`).
struct Parts {
    _objs: ChargedVec<Box<WinObj>>,
    _table: ChargedVec<u64>,
    reader: usize,
    claims: PartitionRange,
    /// D3336 (A1a, lowerv4 face/partfill.rs, `WINDOW_CUT`): the fill runs as claims too, one object each (its source reads
    /// the claim's share of K-cut parts into that object, kernels `agg_runs_claim`).
    fill: Option<(usize, PartitionRange)>,
}

/// The reader's claims of a parted window's `n` objects: one object each (a boundary after every object, so the pool's
/// sizer never hands two to one claim; kernels `m10_win_part` raises by name at a claim of several).
fn object_claims(n: u64, mut bounds: ChargedVec<u64>) -> PartitionRange {
    bounds.with_vec(|v| v.extend(1..n)); // alloc: Exec charged bounds.owner()
    PartitionRange(RangeCursor::with_boundaries(n, ClaimSize::Fixed(1), &bounds))
}

impl Live for WinLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::DEFERRED | crate::ops::ask::FRAME | crate::ops::ask::PULL_READERS | crate::ops::ask::CLAIMS
    }

    /// D3336: a parted window's reader claims its objects; any other window answers none (`State::claims_for` reads on to
    /// the next live).
    fn claims_for(&self, pipeline: usize) -> Option<&dyn MorselSource> {
        let parts = self.parts.as_ref()?;
        match &parts.fill {
            Some((fill, claims)) if *fill == pipeline => Some(claims as &dyn MorselSource),
            _ => (parts.reader == pipeline).then_some(&parts.claims as &dyn MorselSource),
        }
    }

    fn deferred(&self, pipeline: usize) -> bool {
        self.pulled.as_ref().is_some_and(|(p, ..)| u16::try_from(pipeline).is_ok_and(|x| p.members.contains(&x)))
    }

    fn frame(&self, f: Option<crate::ops::Frame>) {
        if let Some((p, ..)) = &self.pulled {
            p.frame.set(f);
        }
    }

    fn pull_readers(&self, pipeline: usize) -> Option<&[usize]> {
        self.pulled.as_ref().filter(|(_, k, r)| *k == pipeline && !r.is_empty()).map(|(_, _, r)| &r[..])
    }
}

/// One pull of the fill: true when it stopped after the rows the store wants, false once it ended (sticky); an error
/// once is every later pull's, by name.
fn pull(p: &Pull) -> PgResult<bool> {
    if p.done.get() {
        return Ok(false);
    }
    if p.failed.get() {
        return Err(glue::internal("gravity: window.agg's pulled fill read again after its error (M8 RT-2f)"));
    }
    if p.running.get() {
        return Err(glue::internal("gravity: window.agg's fill pulled inside its own run"));
    }
    let Some(f) = p.frame.get() else {
        return Err(glue::internal("gravity: window.agg's pull has no runner frame (the lowering pins the reader to the leader)"));
    };
    p.running.set(true);
    let r = super::raw::pull(f, &p.members);
    p.running.set(false);
    match r {
        Ok(gravity_driver::Pulled::Done) => {
            p.done.set(true);
            Ok(false)
        }
        Ok(gravity_driver::Pulled::Stopped) => Ok(true),
        Err(e) => {
            p.failed.set(true);
            Err(glue::pg_error(e))
        }
    }
}

/// D4237 (PG's ExecReScanWindowAgg): the pulled fill re-armed by the run of a rerun set holding it (exec rerun/mod.rs `run`,
/// ops/window/raw.rs `rearm_windows`, the object's `WinObj::rearm` first): its answers start again (no end, no error) and
/// the driver's pull state and kept slots of its pulled set are reset through the runner's frame
/// (`OnDemand::rearm_pulled`), so the next pull runs the fill from its start. A re-arm inside its own pull is a Bug by name.
fn rearm(p: &Pull) -> PgResult<()> {
    if p.running.get() {
        return Err(glue::internal("gravity: window.agg's fill re-armed inside its own pull (D4237)"));
    }
    let Some(f) = p.frame.get() else {
        return Err(glue::internal("gravity: window.agg's re-arm has no runner frame (the lowering pins the reader to the leader)"));
    };
    p.done.set(false);
    p.failed.set(false);
    super::raw::rearm(f, &p.members).map_err(glue::pg_error)
}

/// The pulled fill's set: pipeline `k` and its exclusive producers, in program order, the fill last.
fn members(program: &gravity_program::Program, k: usize, mut out: ChargedVec<u16>) -> PgResult<ChargedVec<u16>> {
    let mut m: Vec<u16> = program.members(k).members; // alloc: Exec
    m.sort_unstable();
    m.dedup();
    match m.last() == u16::try_from(k).ok().as_ref() {
        true => {
            out.with_vec(|v| {
                v.reserve_exact(m.len()); // alloc: Exec charged out.owner()
                v.extend_from_slice(&m); // alloc: Exec charged out.owner()
            });
            Ok(out)
        }
        false => Err(glue::internal("gravity: window.agg's pulled set does not end in its fill")),
    }
}

/// D1354: the proto arg's function count (bits 40..56, lowerv4 window.rs `handle`) against prepare's own list.
fn check_count(arg: u64, n: usize) -> PgResult<()> {
    match (arg >> 40) as u16 as usize == n {
        true => Ok(()),
        false => Err(glue::internal("gravity: window.agg's function list at prepare is not lowering's (their VOLATILE readings differ)")),
    }
}

impl Protocol for Window {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Window
    }

    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let base = p.words.base as usize;
        if p.words.n != 1 || base >= cx.words.len() || base >= cx.nulls.len() {
            return Err(glue::internal("gravity: window.agg's declaration is not its object's (one handle word)"));
        }
        let pstmt = cx.admitted.pstmt.ok_or_else(|| glue::internal("gravity: window.agg without its plan"))?;
        let plan: &'static WindowAgg<'static> = plan_node(pstmt, p.arg as u32 as i32).and_then(|n| n.as_window_agg()).ok_or_else(|| glue::internal("gravity: window.agg's node is no WindowAgg of the plan"))?;
        let live = match p.arg & gravity_program::protos::WINDOW_PARTED != 0 {
            true => parted(p, cx, plan)?,
            false => single(p, cx, pstmt, plan)?,
        };
        Ok(Some(Box::new(live))) // alloc: Exec
    }
}

/// The execution's one window object (every window but a parted one), its handle laid into the proto's word.
fn single(p: &Proto, cx: &mut Prepare<'_>, pstmt: &'static PlannedStmt<'static>, plan: &'static WindowAgg<'static>) -> PgResult<WinLive> {
    let base = p.words.base as usize;
    let mut spec = spec(p, cx, pstmt, plan)?;
    let pulled = match p.arg & gravity_program::protos::WINDOW_PULLED != 0 {
        true => {
            let k = p.pipeline as usize;
            if k >= cx.program.pipelines.len() {
                return Err(glue::internal("gravity: window.agg's pulled fill is none of the program's pipelines"));
            }
            let set = Rc::new(Pull { members: members(cx.program, k, cx.charged_vec())?, frame: Cell::new(None), running: Cell::new(false), done: Cell::new(false), failed: Cell::new(false) }); // alloc: Exec
            let mut readers = cx.charged_vec();
            readers.with_vec(|v| v.extend(cx.program.consumers_of(gravity_program::LifeId(p.pipeline)))); // alloc: Exec charged cx.owner
            Some((set, k, readers))
        }
        false => None,
    };
    let pull_fn: Option<Box<dyn FnMut() -> PgResult<bool>>> = pulled.as_ref().map(|(s, ..)| {
        let s = s.clone();
        Box::new(move || pull(&s)) as Box<dyn FnMut() -> PgResult<bool>> // alloc: Exec
    });
    // D4237: the run of a rerun set holding the fill re-arms it (`rearm`)
    let rearm_fn: Option<Box<dyn FnMut() -> PgResult<()>>> = pulled.as_ref().map(|(s, ..)| {
        let s = s.clone();
        Box::new(move || rearm(&s)) as Box<dyn FnMut() -> PgResult<()>> // alloc: Exec
    });
    // W10: a streaming fill's pulls read ahead, the budget doubling from one row
    let grow = pull_fn.is_some() && p.arg & gravity_program::protos::WINDOW_AHEAD != 0;
    (spec.pull, spec.rearm, spec.grow) = (pull_fn, rearm_fn, grow);
    let obj = WinObj::new(spec)?;
    cx.words[base] = obj.handle();
    cx.nulls[base] = 0;
    *cx.nparams = (*cx.nparams).max(base + 1);
    Ok(WinLive { _obj: Some(obj), parts: None, pulled })
}

/// D3336 (win.partruns): a window read by claims: `PARTRUNS_OBJECTS_PER_WORKER` objects per thread of its reader
/// `p.peer` (at most `PARTRUNS_OBJECTS_MAX`), each `parted`, and the handle word holding their table (`WINDOW_PARTS_*`):
/// the serial fill `p.pipeline` stores each partition whole into the object whose turn it is (kernels `m10_win_put_part`),
/// the reader's claims read one object each (`Parts`: this execution answers them). One object, read by the
/// reader on one thread, when a function the objects call is not `pool_safe` or the reader did not lower eligible: the
/// serial form's rows in one claim's object.
fn parted(p: &Proto, cx: &mut Prepare<'_>, plan: &'static WindowAgg<'static>) -> PgResult<WinLive> {
    use gravity_program::cost::{PARTRUNS_OBJECTS_MAX, PARTRUNS_OBJECTS_PER_WORKER};
    let base = p.words.base as usize;
    let pstmt = cx.admitted.pstmt.ok_or_else(|| glue::internal("gravity: window.agg without its plan"))?;
    let (fill, reader) = (p.pipeline as usize, p.peer as usize);
    // lowerv4 window.rs's ARG_LAZY (bit 32) and ARG_DEFER (bit 35): a parted window evaluates every cell at its fill
    if p.arg & (gravity_program::protos::WINDOW_PULLED | 1 << 32 | 1 << 35) != 0 || fill >= cx.program.pipelines.len() || reader >= cx.program.pipelines.len() {
        return Err(glue::internal("gravity: a parted window.agg that is pulled, lazy or names no fill and reader (D3336)"));
    }
    let first = spec(p, cx, pstmt, plan)?;
    let safe = cx.program.pipelines[reader].is_eligible() && !first.lazy && gravity_hosts::ops::window::object::pool_safe(plan, &first.funcs, &first.argtypes, &first.moving_ok)?;
    let width = cx.widths.get(reader).copied().unwrap_or(0) as usize;
    let n = match safe && width >= 2 {
        true => (width * PARTRUNS_OBJECTS_PER_WORKER).min(PARTRUNS_OBJECTS_MAX),
        false => {
            if let Some(w) = cx.widths.get_mut(reader) {
                *w = (*w).min(1);
            }
            // D3336 (A1a): a K-cut fill's one claim, on one thread as its reader
            if let Some(w) = cx.widths.get_mut(fill).filter(|_| p.arg & gravity_program::protos::WINDOW_CUT != 0) {
                *w = (*w).min(1);
            }
            1
        }
    };
    // the table's words never move once laid (no push after the resize): the handle word holds their address; the last
    // word is the fill's turn (the object it stores into now), 0 to start
    let mut table = cx.charged_vec::<u64>();
    table.with_vec(|v| v.resize(gravity_program::protos::WINDOW_PARTS_AT + n + 1, 0)); // alloc: Exec charged cx.owner
    table[gravity_program::protos::WINDOW_PARTS_COUNT] = n as u64;
    let mut objs = cx.charged_vec::<Box<WinObj>>();
    objs.with_vec(|v| v.reserve_exact(n)); // alloc: Exec charged cx.owner
    let mut next = Some(first);
    for i in 0..n {
        let s = match next.take() {
            Some(s) => s,
            None => spec(p, cx, pstmt, plan)?,
        };
        let obj = WinObj::new(WinSpec { parted: true, ..s })?;
        table[gravity_program::protos::WINDOW_PARTS_AT + i] = obj.handle();
        objs.with_vec(|v| v.push(obj)); // alloc: Exec charged cx.owner (reserved above)
    }
    if n >= 2 {
        fires::fire("exec.win.partruns"); // shapecheck fire signal (D3336): a window read by claims over its objects
    }
    cx.words[base] = table.as_ptr() as u64;
    cx.nulls[base] = 0;
    *cx.nparams = (*cx.nparams).max(base + 1);
    let claims = object_claims(n as u64, cx.charged_vec());
    // D3336 (A1a): a K-cut fill claims the objects as its reader does (one object, one claim, when the reader runs on one
    // thread: that claim reads every part)
    let fill = (p.arg & gravity_program::protos::WINDOW_CUT != 0).then(|| (fill, object_claims(n as u64, cx.charged_vec())));
    if fill.is_some() {
        fires::fire("exec.win.partfill"); // shapecheck fire signal (D3336 A1a): a parted window's fill claims its objects
    }
    Ok(WinLive { _obj: None, parts: Some(Parts { _objs: objs, _table: table, reader, claims, fill }), pulled: None })
}

/// WindowAgg `plan`'s object spec for this execution (an eager fill: the caller sets a pulled fill's pull).
fn spec(p: &Proto, cx: &mut Prepare<'_>, pstmt: &'static PlannedStmt<'static>, plan: &'static WindowAgg<'static>) -> PgResult<WinSpec> {
    let child = plan.plan.lefttree.and_then(|c| c.as_plan()).ok_or_else(|| glue::internal("gravity: a WindowAgg without its child"))?;
    // D1354: an equal VOLATILE window function is its own entry (PG's contain_volatile_functions, as lowering's facts
    // read it); lowering's count rides in the arg's bits 40..56, a different reading an internal error, never a layout
    let nodes = gravity_decompose::ops::window::window_funcs(plan, &crate::effects::contains_volatile);
    check_count(p.arg, nodes.len())?;
    let natts = child.targetlist.len();
    let args = gravity_decompose::ops::window::arg_cells(&nodes, natts);
    // E-kern: the spec's lists (plan-sized, built once per prepare) on this execution's prepare owner (`cx.owner`): the
    // WinObj holding them lives in this execution's `WinLive`, never in a cached product
    let mut cols = cx.charged_vec::<(i16, bool)>();
    for te in child.targetlist.iter() {
        let e = te.as_target_entry().map_or(te, |t| t.expr);
        let c = gravity_fn::typesem::storage::len_byval(expr_type::type_of(e).unwrap_or_else(|| expr_type::result_type(e)))?;
        cols.with_vec(|v| v.push(c)); // alloc: Exec charged cx.owner
    }
    let (mut funcs, mut argtypes, mut moving_ok, mut builtins) = (cx.charged_vec(), cx.charged_vec(), cx.charged_vec(), cx.charged_vec());
    // D1232: every argument cell lazy (a VOLATILE argument); D3584: the `deferred_arg` ones (lowerv4 window `lazy_cells`)
    let (all_lazy, defer) = (p.arg >> 32 & 1 == 1, p.arg >> 35 & 1 == 1);
    let mut lazy_cells = cx.charged_vec::<bool>();
    lazy_cells.with_vec(|v| v.resize(natts, false)); // alloc: Exec charged cx.owner
    for n in nodes.iter() {
        let f = n.as_window_func().ok_or_else(|| glue::internal("gravity: window.agg's function list holds another node"))?;
        let mut types = cx.charged_vec::<Oid>();
        types.with_vec(|v| v.extend(f.args.iter().map(|e| expr_type::type_of(e).unwrap_or_else(|| expr_type::result_type(e))))); // alloc: Exec charged cx.owner
        for (a, &t) in types.iter().enumerate() {
            let (len, byval) = gravity_fn::typesem::storage::len_byval(t)?;
            cols.with_vec(|v| v.push((len, byval))); // alloc: Exec charged cx.owner
            let deferred = defer && gravity_decompose::ops::window::deferred_arg(f, a, lsyscache::getBaseType(t)?, byval);
            lazy_cells.with_vec(|v| v.push(all_lazy || deferred)); // alloc: Exec charged cx.owner
        }
        if f.aggfilter.is_some() {
            cols.with_vec(|v| v.push((1, true))); // alloc: Exec charged cx.owner
            lazy_cells.with_vec(|v| v.push(all_lazy)); // alloc: Exec charged cx.owner
        }
        // D1233: a user internal WINDOW function runs as the windowfuncs.c builtin fmgr resolves it to (lowering admitted it)
        let builtin = match f.winagg || (3100..=3114).contains(&f.winfnoid) {
            true => f.winfnoid,
            false => <gravity_expr::PgCatalog as gravity_expr::Catalog<'_>>::func(&gravity_expr::PgCatalog, f.winfnoid)?.body,
        };
        builtins.with_vec(|v| v.push(builtin)); // alloc: Exec charged cx.owner
        funcs.with_vec(|v| v.push(f)); // alloc: Exec charged cx.owner
        argtypes.with_vec(|v| v.push(types)); // alloc: Exec charged cx.owner
        let ok = !clauses::contain_volatile_functions(*n)? && !clauses::contain_subplans(*n)?;
        moving_ok.with_vec(|v| v.push(ok)); // alloc: Exec charged cx.owner
    }
    let off = |o: Option<Node<'static>>| o.map_or(Ok((8, true)), |e| gravity_fn::typesem::storage::len_byval(expr_type::type_of(e).unwrap_or_else(|| expr_type::result_type(e))));
    let offsets = [off(plan.startOffset)?, off(plan.endOffset)?];
    // D1355: a lazy argument's `$n` reads this execution's list (a PL list is a materialized image: every value fetched)
    let h = cx.admitted.params;
    let params: Box<[types_portal::params::ParamExternData]> = match h.is_null() {
        true => Box::new([]), // alloc: Exec
        false => types_portal::params::with(h, |ps| ps.into()), // alloc: Exec
    };
    let lazy = all_lazy || lazy_cells.contains(&true);
    // W3: a lazy argument's PARAM_EXEC reads the object's own array, one cell per the statement's executor parameter
    let n_exec = if lazy { pstmt.paramExecTypes.len() } else { 0 };
    let fetch = types_portal::params::fetch_hook(h);
    Ok(WinSpec { plan, funcs, cols, natts, args, argtypes, moving_ok, builtins, offsets, lazy, lazy_cells, params, fetch, n_exec, init_param: crate::rerun::init_param, pull: None, rearm: None, grow: false, parted: false })
}

/// The plan node numbered `id` in the statement (the main tree or a subplan; exec v4/m6.rs's walk).
fn plan_node(pstmt: &'static PlannedStmt<'static>, id: i32) -> Option<Node<'static>> {
    fn under(n: Option<Node<'static>>, id: i32) -> Option<Node<'static>> {
        stack_depth_core::check_stack_depth_or_panic();
        let n = n?;
        let p = n.as_plan()?;
        if p.plan_node_id == id {
            return Some(n);
        }
        let kids = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|m| &m.mergeplans)];
        under(p.lefttree, id).or_else(|| under(p.righttree, id)).or_else(|| kids.into_iter().flatten().find_map(|l| l.iter().find_map(|c| under(Some(c), id)))).or_else(|| n.as_subquery_scan().and_then(|s| under(s.subplan, id)))
    }
    under(pstmt.planTree, id).or_else(|| pstmt.subplans.iter().find_map(|s| under(s, id)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_count_prepare_does_not_share_is_an_internal_error() {
        let arg = 7u64 | 1 << 32 | 3u64 << 40;
        assert!(super::check_count(arg, 3).is_ok());
        let e = super::check_count(arg, 2).expect_err("a list one shorter than lowering's");
        assert!(format!("{e:?}").contains("VOLATILE readings differ"), "{e:?}"); // alloc: Cold
        assert!(super::check_count(7, 1).is_err(), "no count laid: never taken as a match");
    }
    #[test]
    fn a_parted_windows_reader_claims_one_object_each() {
        use gravity_parallel::MorselSource;
        let c = super::object_claims(4, memledger::ChargedVec::new(None, memledger::Kind::Region));
        let got: Vec<_> = std::iter::from_fn(|| c.claim()).map(|r| (r.lo, r.hi)).collect(); // alloc: Cold
        assert_eq!(got, [(0, 1), (1, 2), (2, 3), (3, 4)]);
    }
}
