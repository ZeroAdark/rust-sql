//! merge.join's FULL merge whose inner raises only where PG's statement fails too (D2212, Hibernate's json_mergepatch
//! emulation: res's seed full-joins the document unions' CTE stores under a GroupAggregate over a Sort, read by an EXPR
//! SubLink's InitPlan, and its recursive term full-joins them again under a hash join that may skip them). PG's FULL
//! merge run to its end reads both its sides whole: INITIALIZE_OUTER fetches the outer's first tuple, INITIALIZE_INNER or
//! ENDOUTER the inner's (a Sort reads its whole input there), and the join hands on every row of either side. The
//! inner's fill runs whole first, in program order, when the effects home's cover takes each filler
//! (effects/visible/cover.rs): an error the inner raises first is then PG's failure too, whichever error comes first. A
//! helper file (ROWS = NONE). Owner M11-MERGE.
use mcx::{GrowIn, PgVec};
use crate::frame::px::Px;

/// Whether FULL merge `k`'s inner fill (`fillers`, a full Sort's run: `full_sort`) runs whole first (the file's head): no
/// filler is a CTE's store or in a rerun set, and the cover takes each one. `Some`: taken, with the licences its fillers
/// hold (construct C2, `Merge.ahead`). A filler whose cover rests on PG's statement running it (not on the root's client)
/// holds the exact licence of merge join `node` (D1, `Proof::pg_fails_too`); one whose cover rests on the root's client
/// reading every row holds the whole-read licence (`effects::whole_read`: the cover holds only in a statement that
/// neither acts nor calls user code, so a filler at most raises).
pub(crate) fn full_whole<'g>(px: &mut Px<'g, '_>, k: usize, node: i32, full_sort: bool, fillers: &[usize]) -> Option<PgVec<'g, gravity_decompose::effects::Licence>> {
    let gx = px.gx;
    if !full_sort || fillers.iter().any(|&x| gx.cte_write(x) || crate::effects::visible::cover::rerun(gx, x)) {
        return None;
    }
    let mcx = px.mcx();
    let mut covers: PgVec<'_, (usize, bool)> = mcx::reserved_in(fillers.len(), mcx);
    for &x in fillers {
        covers.push_in((x, crate::effects::visible::cover::raises_covered(gx, px.arena, x)?));
    }
    let mut held: PgVec<'g, gravity_decompose::effects::Licence> = mcx::reserved_in(covers.len(), mcx);
    if covers.iter().any(|&(_, r)| r) {
        px.whole_read(k);
        if let Ok(l) = gravity_decompose::effects::whole_read(gravity_decompose::effects::Effects::RAISES) {
            for &(x, _) in covers.iter().filter(|&&(_, r)| r) {
                px.hold(x, &l);
            }
            held.push_in(l);
        }
    }
    // D1 exact (PgFailsToo), PG 18.6 ExecMergeJoin: a FULL join run to its end reads both sides whole (MJ_ENDOUTER /
    // MJ_ENDINNER's fill), and the statement runs it so or fails: each raise of a filler covered so is PG's too. The cover
    // holds only in a statement that neither acts nor calls user code, so what a filler runs at most raises
    let proof = gx.graph.stmt.and_then(|s| gravity_decompose::effects::Proof::pg_fails_too(s, node));
    for &(x, _) in covers.iter().filter(|&&(_, r)| !r) {
        // no proof (its node is no Hash Join / FULL Merge Join PG's plan holds): the accepted class, as code that at most
        // raises
        if let Some(l) = proof.and_then(|p| gravity_decompose::effects::exact(gravity_decompose::effects::Effects::RAISES, p).ok()).or_else(|| px.accept(x, gravity_decompose::effects::Effects::RAISES)) {
            px.hold(x, &l);
            held.push_in(l);
        }
    }
    fires::fire("merge.inner.full_whole"); // shapecheck fire signal (D2212: a FULL merge's inner PG's statement covers)
    Some(held)
}
