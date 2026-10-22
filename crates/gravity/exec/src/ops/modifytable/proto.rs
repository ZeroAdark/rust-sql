//! M4's runtime protocols (M4-plan.md §3.11, cards/M4.md L2, Decision 15): the four write protocols v4 declares
//! (`program/src/v4.rs` `ProtoKind::{Literals, Route, Transition}`) and the runner did not build.
//! - `write.literals` (H16a; NEW-29): a VALUES list's Const cells as a per-execution table, so the program is keyed by
//!   shape and never bakes a literal. Each execution's prepare copies the ValuesScan's Const cells in row order
//!   (ValuesNext's) into Exec-lived arrays the declared words point at, and evaluates nothing: a cell that is not a Const
//!   is the program's code, its row marked (`LITERALS_CODED` in the row's first null byte). Face (the reader is
//!   write/literals.rs, M4-INSERT, L3 / L4): `words` = LIT_WORDS words at `words.base` (the cells, the null bytes, the row
//!   count); `arg` = the ValuesScan's plan node id | columns << 32 | the coded cells' `literals_mark` << 48. A plan whose
//!   list marks other cells than the program's is refused at admission (`admit_literals`), a bug's wall at prepare.
//!   CAP-29, the stream form (`LIT_STREAM_WORDS` words): a list whose rows holding an expression pass the per-row code's
//!   capacity streams those rows: word `LIT_STREAM` points at the execution's `raw::Stream` (protos `LiteralsStream`), and
//!   the program calls it at each coded row's turn, PostgreSQL's ValuesNext over that row into the table.
//! - `write.route` (H16b / H27) and `write.transition` own no exec state: the leaf hosts and the
//!   last-leaf cache are `hosts/src/write_route.rs`'s, the transition tuplestores the AFTER-trigger query's
//!   (`after_query.rs`, given back at AfterTriggerEndQuery). Their prepare checks the declaration against the program's write plan: a mismatch is
//!   a bug's wall, never a served statement's (the lowerer refuses first).
//! Created by M4 (owner M4-HOSTS).
use gravity_program::protos::{literals_mark, Proto, ProtoKind, LITERALS_CODED};
use gravity_program::{Cmd, WritePlan};
use types_error::PgResult;
use types_nodes::{Node, NodeList};

use crate::admit::{refuse, Refused};
use crate::ops::{Live, Prepare, Protocol};
use crate::glue;

/// `write.literals`' words: the cells (row-major `u64`), the null bytes (one per cell), the row count.
pub const LIT_CELLS: usize = 0;
pub const LIT_NULLS: usize = 1;
pub const LIT_ROWS: usize = 2;
pub const LIT_WORDS: usize = 3;
/// CAP-29: the stream form's fourth word, the `LiteralsStream` hook, and its word count.
pub const LIT_STREAM: usize = 3;
pub const LIT_STREAM_WORDS: usize = 4;

pub struct Literals;
pub struct Route;
pub struct Transition;

/// A declaration against the program's write plan: `Err` names what does not hold.
pub fn check(p: &Proto, w: &WritePlan, nwords: usize) -> Result<(), &'static str> {
    match p.kind {
        ProtoKind::Literals if !matches!(p.words.n as usize, LIT_WORDS | LIT_STREAM_WORDS) || p.words.base as usize + p.words.n as usize > nwords => Err("write.literals' words are not its table's"),
        ProtoKind::Literals if lit_cols(p) == 0 => Err("write.literals declares no column"),
        ProtoKind::Route if w.route.is_none() => Err("write.route on a write plan without a route"),
        ProtoKind::Transition if w.cmd == Cmd::Select => Err("write.transition on a program that does not write"),
        _ => Ok(()),
    }
}

fn checked(p: &Proto, cx: &Prepare<'_>) -> PgResult<()> {
    check(p, &cx.program.write, cx.words.len()).map_err(|why| glue::internal(&format!("gravity: {why}"))) // alloc: Cold
}

/// write.literals' width and coded-cell mark (`Proto::arg`).
fn lit_cols(p: &Proto) -> usize {
    (p.arg >> 32) as u16 as usize
}

fn lit_mark(p: &Proto) -> u16 {
    (p.arg >> 48) as u16
}

/// The execution's literal table: the arrays the words point at, alive while the execution runs.
struct Table {
    /// CAP-29: the stream form's evaluator (the `LIT_STREAM` word points at its hook), dropped before the arrays it writes.
    _stream: Option<Box<super::raw::Stream>>,
    _cells: memledger::ChargedVec<u64>,
    _nulls: memledger::ChargedVec<u8>,
}

impl Live for Table {
    fn answers(&self) -> u16 {
        crate::ops::ask::NONE
    }
}

impl Protocol for Literals {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Literals
    }

    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        checked(p, cx)?;
        let (id, cols) = (p.arg as u32 as i32, lit_cols(p));
        let pstmt = cx.admitted.pstmt.ok_or_else(|| glue::internal("gravity: write.literals without its plan"))?;
        let Some(vs) = roots(pstmt).find_map(|r| plan_node(r, id)).and_then(|n| n.as_values_scan()) else {
            return Err(glue::internal("gravity: write.literals' node is not a ValuesScan"));
        };
        let lists: &NodeList<'static> = &vs.values_lists;
        let rows = lists.len();
        // the execution's cells and null bytes, one per cell of its VALUES rows (data-sized): on the prepare's owner (E1:
        // the executing portal's or the running statement's), for this execution's Live only; sized once here (their
        // addresses go into the program's words below), never grown after
        let (mut cells, mut nulls) = (cx.charged_vec::<u64>(), cx.charged_vec::<u8>());
        cells.with_vec(|v| *v = vec![0u64; rows * cols]); // alloc: Exec charged cx.owner
        nulls.with_vec(|v| *v = vec![1u8; rows * cols]); // alloc: Exec charged cx.owner
        if fill(lists, cols, &mut cells, &mut nulls)? != lit_mark(p) {
            return Err(glue::internal("gravity: write.literals' list codes other cells than its program"));
        }
        let (base, n) = (p.words.base as usize, p.words.n as usize);
        // CAP-29: the stream form evaluates its coded rows at their turn, `$n` from this execution's list
        let stream = match n == LIT_STREAM_WORDS {
            true => {
                let h = cx.admitted.params;
                let mut params = cx.charged_vec::<types_portal::params::ParamExternData>();
                if !h.is_null() {
                    params.with_vec(|v| types_portal::params::with(h, |ps| v.extend_from_slice(ps))); // alloc: Exec charged cx.owner
                }
                let s = super::raw::Stream::new(lists, cols, &mut cells, &mut nulls, params, types_portal::params::fetch_hook(h), cx.mcx, cx.owner)?;
                cx.words[base + LIT_STREAM] = s.hook();
                Some(s)
            }
            false => None,
        };
        cx.words[base + LIT_CELLS] = cells.as_ptr() as u64;
        cx.words[base + LIT_NULLS] = nulls.as_ptr() as u64;
        cx.words[base + LIT_ROWS] = rows as u64;
        cx.nulls[base..base + n].fill(0);
        *cx.nparams = (*cx.nparams).max(base + n);
        Ok(Some(Box::new(Table { _stream: stream, _cells: cells, _nulls: nulls }))) // alloc: Exec
    }
}

/// The statement's plan trees: the main tree, then each subplan's (a data-modifying CTE's VALUES sits under its InitPlan's
/// ModifyTable: D1669).
pub fn roots<'p>(pstmt: &'p types_nodes::plannodes::PlannedStmt<'p>) -> impl Iterator<Item = Option<Node<'p>>> + 'p {
    core::iter::once(pstmt.planTree).chain(pstmt.subplans.iter())
}

/// R2b (V4R-205): write.literals' admission over this plan's trees, at every admission of the statement (admit.rs `bind`,
/// through `m4::admit`: a lowered, cached or front-hit program alike). The declared ValuesScan's rows must be the program's
/// width and its cells that are not a Const the program's own (`literals_mark`), else a named refusal: a program keyed on a
/// Const cell that meets a plan holding a `$1` there is refused here, before any execution, and the statement takes the
/// row executor.
pub fn admit_literals<'p>(p: &Proto, trees: impl IntoIterator<Item = Option<Node<'p>>>) -> Result<(), Refused> {
    let (id, cols) = (p.arg as u32 as i32, lit_cols(p));
    let Some(vs) = trees.into_iter().find_map(|r| plan_node(r, id)).and_then(|n| n.as_values_scan()) else {
        return refuse("v4", "write.literals' node is not a ValuesScan");
    };
    match coded_mark(&vs.values_lists, cols) {
        Some(m) if m == lit_mark(p) => Ok(()),
        Some(_) => refuse("v4", "write.literals over a list whose cells that are not a constant are not the program's"),
        None => refuse("v4", "write.literals over a row of another width"),
    }
}

/// The `literals_mark` of the list's cells that are not a Const; `None` when a row is not `cols` wide.
fn coded_mark(lists: &NodeList<'_>, cols: usize) -> Option<u16> {
    if !lists.iter().all(|row| row.as_list().is_some_and(|l| l.len() == cols)) {
        return None;
    }
    let coded = lists.iter().enumerate().flat_map(|(r, row)| {
        row.as_list().into_iter().flat_map(|l| l.iter().enumerate()).filter(|(_, e)| e.as_const().is_none()).map(move |(c, _)| (r, c))
    });
    Some(literals_mark(coded))
}

/// write.literals as the crate rig lays it (lowerv4 tests common/on_demand.rs, beside Init's `prepare_bare`): each declared
/// table filled from its pipeline's VALUES lists (`lists`, the decomposed graph's), the words set as `prepare` sets them.
pub fn prepare_bare<'p>(program: &gravity_program::Program, lists: impl Fn(usize) -> Option<&'p NodeList<'p>>, words: &mut [u64], nulls: &mut [u8], nparams: &mut usize, out: &mut crate::ops::LiveList) -> PgResult<()> {
    let protos = program.v4.as_ref().map_or(&[][..], |d| &d.protos[..]);
    for p in protos.iter().filter(|p| p.kind == ProtoKind::Literals) {
        let l = lists(p.pipeline as usize).ok_or_else(|| glue::internal("gravity: write.literals over a pipeline that is not a VALUES"))?;
        let (cols, base) = (lit_cols(p), p.words.base as usize);
        if base + LIT_WORDS > words.len() {
            return Err(glue::internal("gravity: write.literals' words are not its table's"));
        }
        if p.words.n as usize == LIT_STREAM_WORDS {
            // CAP-29: the stream form evaluates rows with PostgreSQL's executor, which the crate rig does not run
            return Err(glue::internal("gravity: write.literals' stream form outside an execution"));
        }
        // no execution here (the crate rig): uncharged
        let (mut cells, mut nb) = (memledger::ChargedVec::<u64>::none(), memledger::ChargedVec::<u8>::none());
        cells.with_vec(|v| *v = vec![0u64; l.len() * cols]); // alloc: Exec
        nb.with_vec(|v| *v = vec![1u8; l.len() * cols]); // alloc: Exec
        if fill(l, cols, &mut cells, &mut nb)? != lit_mark(p) {
            return Err(glue::internal("gravity: write.literals' list codes other cells than its program"));
        }
        words[base + LIT_CELLS] = cells.as_ptr() as u64;
        words[base + LIT_NULLS] = nb.as_ptr() as u64;
        words[base + LIT_ROWS] = l.len() as u64;
        nulls[base..base + LIT_WORDS].fill(0);
        *nparams = (*nparams).max(base + LIT_WORDS);
        let l = Box::new(Table { _stream: None, _cells: cells, _nulls: nb }); // alloc: Exec
        out.with_vec(|v| v.push(l)); // alloc: Exec charged out
    }
    Ok(())
}

/// Every Const cell in row order (ValuesNext's), its datum the plan's, alive while the execution runs; a cell that is not
/// a Const is evaluated by the program's code in its row's turn, never here, its row marked `LITERALS_CODED`. Returns the
/// coded cells' `literals_mark`.
fn fill(lists: &NodeList<'_>, cols: usize, cells: &mut [u64], nulls: &mut [u8]) -> PgResult<u16> {
    for (r, row) in lists.iter().enumerate() {
        let Some(row) = row.as_list().filter(|l| l.len() == cols) else {
            return Err(glue::internal("gravity: a write.literals row is not the table's width"));
        };
        for (c, e) in row.iter().enumerate() {
            match e.as_const() {
                Some(k) => {
                    cells[r * cols + c] = k.constvalue.as_u64();
                    nulls[r * cols + c] = k.constisnull as u8;
                }
                None => nulls[r * cols] |= LITERALS_CODED,
            }
        }
    }
    Ok(coded_mark(lists, cols).unwrap_or(0))
}

macro_rules! host_owned {
    ($($t:ident => $k:ident),*) => {$(
        impl Protocol for $t {
            fn kind(&self) -> ProtoKind {
                ProtoKind::$k
            }

            fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
                checked(p, cx).map(|()| None)
            }
        }
    )*};
}

host_owned!(Route => Route);

impl Protocol for Transition {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Transition
    }

    /// Host-owned as Route; D2605 EARLY: the EARLY_AS bit names the CTE writers' relations (early_as.rs).
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        checked(p, cx).map(|()| super::early_as::prepare(p, cx)).map(|()| None)
    }
}

/// The plan node numbered `id` under `root` (a ModifyTable's subplan is its lefttree; a VALUES may sit under a Result, a
/// SubqueryScan or an Append / MergeAppend arm).
fn plan_node<'p>(root: Option<Node<'p>>, id: i32) -> Option<Node<'p>> {
    stack_depth_core::check_stack_depth_or_panic();
    let n = root?;
    let p = n.as_plan()?;
    if p.plan_node_id == id {
        return Some(n);
    }
    let kids = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|m| &m.mergeplans)];
    plan_node(p.lefttree, id)
        .or_else(|| plan_node(p.righttree, id))
        .or_else(|| kids.into_iter().flatten().find_map(|l| l.iter().find_map(|c| plan_node(Some(c), id))))
        .or_else(|| n.as_subquery_scan().and_then(|s| plan_node(s.subplan, id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_program::{Arena, IndexPlan, RoutePlan};

    /// R2b (V4R-205): write.literals' admission, run at every admission of the statement. One declared protocol (as a
    /// cached program carries it: keyed on a Const-only list) meets two plans of the same shape: the Const-only VALUES
    /// admits; the same shape with `$1` in a cell, and a row of another width, are named refusals (never an execute error).
    /// NEW-29: a program that codes that cell (its mark in the arg) admits the `$1` list and refuses the Const-only one.
    #[test]
    fn a_cached_literal_table_admits_constants_only() {
        use types_nodes::plannodes::{Plan, Scan, ValuesScan};
        use types_nodes::primnodes::{Param, ParamKind};
        let ctx: &'static mcx::MemoryContext = Box::leak(Box::new(mcx::MemoryContext::new("t"))); // alloc: Cold
        let m = ctx.mcx();
        let k = |v: i32| Node::mk_const(m, 23, -1, 0, 4, datum::Datum::from_i32(v), false, true).unwrap();
        let list = |cells: &[Node<'static>]| {
            let mut l = NodeList::nil();
            for c in cells {
                l.lappend(m, *c).unwrap();
            }
            l
        };
        let values = |rows: &[&[Node<'static>]]| {
            let lists = list(&rows.iter().map(|r| Node::mk_list(m, list(r)).unwrap()).collect::<Vec<_>>());
            Node::mk(m, ValuesScan { scan: Scan { plan: Plan { plan_node_id: 7, ..Default::default() }, ..Default::default() }, values_lists: lists }).unwrap()
        };
        let param = Node::mk(m, Param { paramkind: ParamKind::PARAM_EXTERN, paramid: 1, paramtype: 23, ..Default::default() }).unwrap();
        let p = proto(ProtoKind::Literals, 0, LIT_WORDS as u16, 7 | 2 << 32);
        assert!(admit_literals(&p, [Some(values(&[&[k(10), k(1)], &[k(11), k(2)]]))]).is_ok(), "a Const-only VALUES");
        let r = admit_literals(&p, [Some(values(&[&[k(10), k(1)], &[param, k(2)]]))]);
        assert!(r.as_ref().is_err_and(|r| r.what.contains("not a constant")), "the same shape with $1 in a cell: {r:?}");
        assert!(admit_literals(&p, [Some(values(&[&[k(10)]]))]).is_err(), "a row of another width");
        assert!(admit_literals(&p, [Some(k(1))]).is_err(), "the declared node is not a ValuesScan");
        // D1669: a data-modifying CTE's VALUES sits in a subplan, after the main tree
        assert!(admit_literals(&p, [Some(k(1)), None, Some(values(&[&[k(10), k(1)]]))]).is_ok(), "a ValuesScan in a later tree");
        let coded = proto(ProtoKind::Literals, 0, LIT_WORDS as u16, 7 | 2 << 32 | (literals_mark([(1, 0)]) as u64) << 48);
        assert!(admit_literals(&coded, [Some(values(&[&[k(10), k(1)], &[param, k(2)]]))]).is_ok(), "the program codes row 1's $1");
        assert!(admit_literals(&coded, [Some(values(&[&[k(10), k(1)], &[k(11), k(2)]]))]).is_err(), "a Const where the program codes");
        assert!(admit_literals(&coded, [Some(values(&[&[param, k(1)], &[k(11), k(2)]]))]).is_err(), "a $1 in another cell");
        let mut cells = [0u64; 4];
        let mut nulls = [1u8; 4];
        let vn = values(&[&[k(10), k(1)], &[param, k(2)]]);
        let vs = vn.as_values_scan().unwrap();
        assert_eq!(fill(&vs.values_lists, 2, &mut cells, &mut nulls).unwrap(), literals_mark([(1, 0)]));
        assert_eq!((cells[0], cells[1], cells[3]), (10, 1, 2), "the Const cells, row-major");
        assert_eq!(nulls, [0, 0, 1 | LITERALS_CODED, 0], "row 1 coded: its $1 is never read from the table");
    }

    fn plan(cmd: Cmd) -> WritePlan {
        let a = Box::leak(Box::new(Arena::new(256))).handle(); // alloc: Cold
        WritePlan::write(a, cmd, 1, &[IndexPlan::new(a, 2, &[1], true)])
    }

    fn proto(kind: ProtoKind, base: u16, n: u16, arg: u64) -> Proto {
        let mut p = Proto::new(kind, 0);
        p.words = gravity_program::protos::Words { base, n };
        p.arg = arg;
        p
    }

    #[test]
    fn literals_take_any_statement_and_their_three_words() {
        let ins = plan(Cmd::Insert);
        // NEW-29: every statement's VALUES lists read the table (an INSERT's own, a SELECT's, an UPDATE's or DELETE's)
        for w in [&ins, &plan(Cmd::Update), &WritePlan::SELECT, &plan(Cmd::Delete)] {
            assert_eq!(check(&proto(ProtoKind::Literals, 4, 3, 7 | 2 << 32), w, 8), Ok(()));
        }
        assert_eq!(check(&proto(ProtoKind::Literals, 4, 3, 7 | 2 << 32 | 9 << 48), &ins, 8), Ok(()), "a coded-cell mark");
        assert!(check(&proto(ProtoKind::Literals, 4, 3, 7 | 9 << 48), &ins, 8).is_err(), "a mark, no column");
        assert!(check(&proto(ProtoKind::Literals, 6, 3, 7 | 2 << 32), &ins, 8).is_err(), "words past the program's");
        assert!(check(&proto(ProtoKind::Literals, 4, 2, 7 | 2 << 32), &ins, 8).is_err(), "not the table's three words");
        // CAP-29: the stream form's fourth word, the hook
        assert_eq!(check(&proto(ProtoKind::Literals, 4, 4, 7 | 2 << 32 | 9 << 48), &ins, 8), Ok(()), "the stream form's four words");
        assert!(check(&proto(ProtoKind::Literals, 5, 4, 7 | 2 << 32), &ins, 8).is_err(), "the stream form's words past the program's");
        assert!(check(&proto(ProtoKind::Literals, 2, 5, 7 | 2 << 32), &ins, 8).is_err(), "five words");
        assert!(check(&proto(ProtoKind::Literals, 4, 3, 7), &ins, 8).is_err(), "no column");
    }

    #[test]
    fn route_takes_only_a_write_plan_with_a_route() {
        let ins = plan(Cmd::Insert);
        let mut routed = plan(Cmd::Insert);
        routed.route = Some(RoutePlan { root: 9, leaves: gravity_program::ArenaVec::new_in(gravity_program::ArenaRef::EMPTY), moves: gravity_program::ArenaVec::new_in(gravity_program::ArenaRef::EMPTY) });
        assert!(check(&proto(ProtoKind::Route, 0, 0, 0), &routed, 0).is_ok());
        assert!(check(&proto(ProtoKind::Route, 0, 0, 0), &ins, 0).is_err(), "G4: a plain INSERT takes no route");
    }

    #[test]
    fn transition_takes_any_write_but_not_a_read() {
        for cmd in [Cmd::Insert, Cmd::Update, Cmd::Delete] {
            assert_eq!(check(&proto(ProtoKind::Transition, 0, 0, 0), &plan(cmd), 0), Ok(()));
        }
        assert!(check(&proto(ProtoKind::Transition, 0, 0, 0), &WritePlan::SELECT, 0).is_err());
    }
}
