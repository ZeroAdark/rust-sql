//! Kind `srf.pl` (L5; cards/M5.md `srf/pl_function.rs`, M5-plan §3.2, M5-ENTRY-SPI §4.1 R1-R7, Decisions 6 / 7): a FROM
//! entry whose functions are all user functions (a PL / non-inlined SQL body, graph.rs; an inlinable SQL SRF is a subquery
//! by the time decompose sees it). The function runs exactly as in PG (Decision 6, the user 09-26): the whole body once,
//! at the entry's first fetch, on the leader, into PG's tuplestore, through the incumbent FunctionScan
//! (`exec_make_table_function_result`: its modes, strict-NULL, the empty store, the row-type check and its text are PG's);
//! the program reads that store in windows (srf/function_store.rs's step over srf/store_read.rs, C2's one-function read).
//! - R2: nothing runs at admission, bind or open: the step's open only takes the store handle, the first window's pull
//!   runs the fill; a LIMIT 0, a cursor never fetched, EXPLAIN without ANALYZE never call.
//! - R3: a stop resumes the read (the offset word), never the call; a rewind is the node's rescan (each store reread,
//!   nothing re-called, volatile or not: `nodeFunctionscan.c:556-611`), so VOLATILE serves (the ladder refused it).
//! - R4: leader only: the FunctionScan's pipeline is `src.result` (hir.rs `Ineligible { reason: Host }`), and the fill
//!   runs at the pipeline's first claim, with no pool task of the statement in flight.
//! - R5: owner and snapshot are the caller's; no Gravity snapshot code. The body's statements nest on trunk's re-entry
//!   levels (R7) and count by E4's metric (§7).
//! - R6: a PARALLEL SAFE function under PG's Gather never reaches here: decompose refuses Gather / Gather Merge by kind.
//! - Decision 7: the program is its session's (its SetFn names the incumbent node: exec admit.rs `shareable`, V4R-258).
//! Owner M5-PL.
use gravity_decompose::ops::shared::srf::SrfPlace;
use gravity_decompose::Op;
use mcx::PgBox;

use crate::face::kinds::KindId;
use crate::face::step::StepMech;
use crate::frame::frame::StepMaker;
use crate::frame::px::Px;
use crate::admit::refusal::Reason;
use crate::ops::Table;

use crate::ops::srf::function_store::store_step;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrfPl, &StepMaker(make))], ..crate::ops::NONE };

fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id);
    let Op::Srf(d) = &p.ops[j] else { crate::refuse!(node, Reason::Bug { payload: "a srf.pl step over another op" }) };
    let SrfPlace::From { scanrelid, ordinality } = d.place else { crate::refuse!(node, Reason::Bug { payload: "a target-list set as srf.pl" }) };
    let ex = crate::frame::iface::expr_face(&node).0;
    let ncols = d.functions.iter().map(|f| f.cols as usize).sum::<usize>() + ordinality as usize;
    store_step(px, ex, (k, j), d, (scanrelid as i32, ordinality, ncols), node)
}
