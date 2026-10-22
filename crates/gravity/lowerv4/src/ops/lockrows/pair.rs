//! D3379 LOCKPAIR (rerun/epq/ref_nest.rs `pair_form`): a LockRows under a LIMIT / OFFSET over an INNER nested loop whose
//! outer and inner relations are both locked (`SELECT .. FROM a JOIN b ON b.id = a.bid .. LIMIT 3 FOR UPDATE`). Each
//! locking mark is a `Program.locks` entry (`LockSite::arg`) over a write site as wide as its relation, positioned at the
//! row's junk ctid of that mark. The marks lock in PlanRowMark order, as ExecLockRows does: a mark answering SKIPPED
//! (deleted concurrently, SKIP LOCKED, modified by this command) drops the row, the marks locked before it staying locked;
//! a negative status is the host's error. When a lock traversed to a newer version (RECHECK: the host put that version's
//! cells and position in its site), each mark whose lock did not is fetched at its position (`LockSite::FETCH`: the
//! version it locked, the one the join read), so every site holds its mark's EvalPlanQual test tuple, and the recheck
//! runs in the IR over the two (epq_ir.rs `epq_of_pair`); a row that passes goes on through the steps above, so the Limit
//! counts it, and one that fails is dropped with its locks kept, as PostgreSQL's. D3379 PAIRPROBE: or that nest in
//! decompose's catalog form, the inner read once into a hash build the outer probes (ref_nest.rs `limited_probe`): the
//! inner's mark is the build's relation, its redo the build's ops before its Build. D3727 PAIRHASH: or PostgreSQL's own
//! Hash Join (realapp-django's `select_for_update()` then `get()` across a relation), its two sides' parts raise-free.
//! D3777 PAIRMERGE: or its Merge Join, either side a Sort over its relation's scan (ref_nest.rs `limited_merge`).
use std::cell::Cell;
use std::rc::Rc;

use gravity_abi::status::{unsupported, UNSUPPORTED_SHAPE, WRITE_RECHECK, WRITE_SKIPPED};
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_decompose::ops::shared::modifytable::LockDesc;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type};
use gravity_program::program::LockSite;
use gravity_program::SiteDesc;
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgRc, PgVec};

use crate::admit::refusal::Reason;
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::ops::lockrows::lower::{junk_tid, store_site_pos};
use crate::rerun::epq::epq_ir::Epq;

/// One locking mark, in lock order: its write site (numbered late), its relation and its junk ctid column in the row.
struct Mark {
    site: Rc<Cell<usize>>,
    relid: u32,
    col: i16,
}

pub(crate) struct LockPair<'g> {
    node: i32,
    marks: PgVec<'g, Mark>,
    /// The outer's mark in `marks`: its site's cells are the recheck's main row.
    outer: usize,
    tid: TypeFacts,
    epq: PgRc<'g, Epq<'g>>,
    mcx: Mcx<'g>,
}

/// The step of lock op `j` of pipeline `k` in `pair_form`'s shape: `io` / `ii` the outer's and the inner's marks in
/// `d.marks`, `jn` the nest op, `ik` its inner's `Pipes` index; `so` a merge outer's Sort producer and its sort build op.
pub(crate) fn lock_pair<'g>(px: &mut Px<'g, '_>, node: i32, d: &LockDesc<'_>, (k, j): (usize, usize), (io, ii, jn, ik, so): (usize, usize, usize, usize, Option<(usize, usize)>)) -> PgBox<'g, dyn StepMech + 'g> {
    let Some(tid) = typefacts::facts_of(px.gx.facts, crate::cell::TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
    px.second_write(node);
    let mcx = px.mcx();
    let mut marks = mcx::reserved_in(d.marks.len(), mcx);
    let mut locks = Vec::with_capacity(d.marks.len()); // alloc: Program
    for m in d.marks.iter() {
        let natts = px.gx.facts.rel(m.relid).map_or(0, |r| r.natts.max(0) as u16);
        marks.push_in(Mark { site: px.add_late_site(SiteDesc::write(m.relid, natts)), relid: m.relid, col: m.ctid_col });
        locks.push(LockSite { rti: m.rti, relid: m.relid, lock: m.lock, wait: m.wait }); // alloc: Program
    }
    let rels = (d.marks[io].relid, d.marks[ii].relid);
    let epq = PgRc::new_in(crate::ops::lockrows::epq::epq_of_pair(px, (k, j), (jn, ik), rels, marks[ii].site.clone(), node, so), mcx);
    px.keep_write(d.plan.copy_in(px.arena));
    crate::ops::lockrows::plan::Plan(px).lay(locks);
    mcx::pg_box_dyn!(mcx, LockPair { node, marks, outer: io, tid, epq, mcx } => dyn StepMech + 'g)
}

impl<'g> StepMech for LockPair<'g> {
    /// The positions are the row's junk ctids, so no source locator is read.
    fn wants_locator(&self) -> bool {
        false
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        for m in self.marks.iter() {
            let pos = junk_tid(b, env, row, (m.col, &self.tid), self.node);
            let rel = b.iconst(Type::I32, m.relid as i32 as i64);
            store_site_pos(b, env, m.site.get(), pos, rel);
        }
        let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
        let lock_row = gravity_kernels::ops::lockrows::entries::lock_row as *const () as usize;
        let h = b.declare_host_once("lock_row", &[Type::Ptr, Type::I64], Some(Type::I64), lock_row, mem);
        let types: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(self.mcx);
        let l_join = b.create_block(&types[..]);
        let l_bad = b.create_block(&[Type::I64]);
        b.mark_cold(l_bad);
        let (rc, sk) = (b.iconst(Type::I64, WRITE_RECHECK), b.iconst(Type::I64, WRITE_SKIPPED));
        // each mark's lock in PlanRowMark order, and whether it traversed to a newer version
        let mut traversed = mcx::reserved_in(self.marks.len(), self.mcx);
        for (i, m) in self.marks.iter().enumerate() {
            let arg = b.iconst(Type::I64, LockSite::arg(m.site.get(), i) as i64);
            let st = b.call_host(h, &[env.ctx, arg], Some(Type::I64)).unwrap_or(env.zero);
            // below zero: the host's error; above RECHECK: no lock answers it
            let odd = b.icmp(IntCond::Sgt, st, rc);
            let neg = b.icmp(IntCond::Slt, st, env.zero);
            let bad = b.int_bin(IntBinOp::Or, neg, odd);
            let (l_ok, l_on) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(bad, (l_bad, &[st]), (l_ok, &[]));
            b.switch_to(l_ok);
            let is_sk = b.icmp(IntCond::Eq, st, sk);
            b.cond_br(is_sk, (l_join, &row.carry.params()), (l_on, &[]));
            b.switch_to(l_on);
            traversed.push_in(b.icmp(IntCond::Eq, st, rc));
        }
        let any = traversed.iter().skip(1).fold(traversed[0], |a, &t| b.int_bin(IntBinOp::Or, a, t));
        let (l_rc, l_done) = (b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_rc);
        b.cond_br(any, (l_rc, &[]), (l_done, &[]));
        b.switch_to(l_rc);
        // a mark whose lock did not traverse: the version it locked fetched into its site, its test tuple
        for (i, (m, &t)) in self.marks.iter().zip(traversed.iter()).enumerate() {
            let (l_fetch, l_next) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(t, (l_next, &[]), (l_fetch, &[]));
            b.switch_to(l_fetch);
            let arg = b.iconst(Type::I64, (LockSite::FETCH | LockSite::arg(m.site.get(), i)) as i64);
            let st = b.call_host(h, &[env.ctx, arg], Some(Type::I64)).unwrap_or(env.zero);
            let neg = b.icmp(IntCond::Slt, st, env.zero);
            b.cond_br(neg, (l_bad, &[st]), (l_next, &[]));
            b.switch_to(l_next);
        }
        let c = self.epq.lower(b, env, row, self.marks[self.outer].site.get(), self.node, &mut *next);
        b.br(l_join, &c.params());
        // no lock traversed: the locked row goes on
        b.switch_to(l_done);
        let c = next(b, env, row);
        b.br(l_join, &c.params());
        b.switch_to(l_bad);
        let st = b.block_params(l_bad)[0];
        let neg = b.icmp(IntCond::Slt, st, env.zero);
        let uns = b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE));
        let err = b.select(neg, st, uns);
        b.br(env.x.l_stop_in, &[row.carry.out, row.at, err]);
        b.switch_to(l_join);
        Carry::from_params(b.block_params(l_join), env.mcx)
    }
}
