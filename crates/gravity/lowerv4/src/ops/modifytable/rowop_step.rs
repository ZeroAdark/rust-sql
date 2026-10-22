//! The row op as a step (face/end.rs `RowOpMech`; M4-plan §1.3 L0-a / L0-b, L0-g; cards/M4.md L0-a / L0-b; the ladder's
//! `write_row` / `store_cell` / `after_write`, lower/src/write.rs at lowerv2-final). M4's write kinds call `make_op`
//! (`rowop_step::make_op`) and call the host in their
//! step, so the row goes on as written and a RETURNING projection after the op reads it; their end wraps `end.sink`.
//! Per row the step stores the target's cells (value, null byte by attno) into the write site and the position: the
//! driving source's locator, the target's junk ctid (a write over a join or an emit: UPDATE ... FROM, DELETE ... USING,
//! MERGE), or none (INSERT). Then ONE host call, `write_row` (write/rowop.rs `host_op`), and a branch on its status:
//! - `WRITE_DONE`: `done` with the row; the written cells (a BEFORE ROW trigger's row, driver dml.rs `written`) are read
//!   back by `cells`.
//! - `WRITE_SKIPPED`: nothing goes on; the carry as it came.
//! - `WRITE_RECHECK`: a stop after the row, `recheck(site)` naming the target's site, as M1's DELETE end.
//! - `WRITE_CONFLICT` (a declared conflict site only): the `conflict` arm; the host has put the existing row in the
//!   conflict site, the write site + 1 (driver dml.rs), which `cells(.., true)` reads.
//! - below zero: the host's error, a stop at the row.
//! The ModifyTable node's loops count at the open, as the end's. Width: leader-only, `claim`'s refusal (write/rowop.rs).
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use gravity_abi::ctx::site_off;
use gravity_abi::site::site;
use gravity_abi::program::{WRITE_DELETE, WRITE_INSERT};
use gravity_abi::status::{recheck, unsupported, UNSUPPORTED_SHAPE, WRITE_CONFLICT, WRITE_DONE, WRITE_NOT_MATCHED, WRITE_RECHECK};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_program::SiteDesc;

use crate::face::end::{Arm, LocFrom, RowOpDecl, RowOpMaker, RowOpMech, RowOpRow, SYS_CELLS};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::instr;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::admit::refusal::Reason;

use crate::ops::modifytable::rowop::{claim, host_op, shared_sys, stop_at, store_loc, store_pos, Late};

/// The step form's maker, one more row of `write.delete` (the kind that owns the row-op machinery at M1).
pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::WriteDelete, &RowOpMaker(make_op))], ..crate::ops::NONE };

/// The row op as a step for pipeline `k`: the write site, then the conflict site right after it.
pub(crate) fn make_op<'g>(px: &mut Px<'g, '_>, k: usize, d: RowOpDecl) -> PgBox<'g, dyn RowOpMech + 'g> {
    let (wants_old, wants_sys, old_n) = (d.plan.old, d.plan.sys, d.natts + d.plan.sys_old as u16 * SYS_CELLS);
    // EXPLAIN ANALYZE's Conflicting Tuples under DO NOTHING: a skipped row is a conflict's where no BEFORE ROW INSERT
    // trigger can skip one (the target's, a routed leaf's), as only ExecInsert's conflict arm counts it
    let nothing = d.op == WRITE_INSERT && d.plan.conflict == gravity_program::Conflict::Nothing && instr::on() && {
        use gravity_decompose::ops::modifytable::trig_facts::TRIG_INSERT;
        let t = px.gx.facts.trig_ext(d.relid);
        t.known && t.before_row & TRIG_INSERT == 0 && d.plan.route.as_ref().is_none_or(|r| r.leaves.iter().all(|l| !l.before_row))
    };
    let (site, node, write_node, marks) = claim(px, k, d.relid, d.natts, d.plan, true);
    let conflict = d.conflict.then(|| px.add_late_site(SiteDesc::write(d.relid, d.natts)));
    // M4-RET R4a: RETURNING OLD's site, the successor of the site the host updates (the write site's; under DO UPDATE the
    // conflict site's, whose update fetches the existing row it replaced): driver dml.rs fills `i + 1`; V4R-501: its
    // system columns after its cells (`WritePlan.sys_old`)
    let old = wants_old.then(|| shared_sys(px, &site, d.relid, old_n, true));
    // D774: RETURNING's system columns (WritePlan.sys): six cells after every other site of the op (driver dml.rs
    // `Writer::sys_off`); `cells` hands them after the written row's
    let sys = wants_sys.then(|| shared_sys(px, &site, d.relid, SYS_CELLS, false));
    let fed = crate::ops::modifytable::rowop_sub::fed(px, k, node);
    let epq = super::rowop_epq::in_ir(px.gx, k);
    mcx::pg_box_dyn!(px.mcx(), RowOpStep { op: d.op, relid: d.relid, natts: d.natts, site, loc: d.loc, conflict, old, old_n, sys, node, write_node, marks, fed, epq, nothing } => dyn RowOpMech + 'g)
}

struct RowOpStep<'g> {
    op: u32,
    relid: u32,
    natts: u16,
    /// The write site and the conflict site after it, numbered at emit (write/rowop.rs `claim`).
    site: Late,
    loc: LocFrom,
    conflict: Option<Late>,
    /// M4-RET R4a: the old site (`WritePlan.old`), the write site's successor; `old_n` cells (V4R-501: + the six sys).
    old: Option<Late>,
    old_n: u16,
    /// D774: the sys site (`WritePlan.sys`: ctid, xmin, cmin, xmax, cmax, tableoid), after the old site.
    sys: Option<Late>,
    node: i32,
    write_node: i32,
    /// D655: the joined relations' marks (write/rowop.rs `claim`), stored as their sites' positions before the host call.
    marks: crate::rerun::epq::marks::Marks<'g>,
    /// W6-D1112: the check options' SubPlans the host parks at (write/rowop_sub.rs).
    fed: crate::ops::modifytable::rowop_sub::Fed<'g>,
    /// D655 Row C (V4R-534): WRITE_RECHECK reruns in the IR (write/rowop_epq.rs), the kind's `Arm::Epq`.
    epq: bool,
    /// An INSERT ... ON CONFLICT DO NOTHING whose skipped rows are its conflicts (EXPLAIN ANALYZE counts them).
    nothing: bool,
}

impl RowOpMech for RowOpStep<'_> {
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        if self.write_node != self.node {
            instr::count_started(b, env.ctx, self.write_node);
        }
        if self.loc == LocFrom::None {
            self.empty_pos(b, env);
        }
    }

    fn call<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, r: RowOpRow<'_>, done: Next<'_, 'e>, conflict: Option<Next<'_, 'e>>) -> Carry<'e> {
        self.call_op(b, env, row, self.op, r, done, conflict.map_or(Arm::None, Arm::Conflict))
    }

    /// A DELETE may pass no cells (the host fills the deleted row for RETURNING); MERGE's INSERT arm (`op` INSERT under
    /// a junk-ctid declaration) passes no tid and writes with an empty position.
    fn call_op<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, op: u32, r: RowOpRow<'_>, done: Next<'_, 'e>, arm: Arm<'_, 'e>) -> Carry<'e> {
        let (arm, rerun) = match arm { Arm::Epq(e, c) if self.epq => (Arm::None, Some((e, c))), Arm::Epq(..) => (Arm::None, None), a => (a, None) };
        let cells_ok = r.cells.len() == self.natts as usize || op == WRITE_DELETE && r.cells.is_empty();
        let junk = matches!(self.loc, LocFrom::JunkCtid { .. });
        let tid_ok = r.tid.is_some() == junk || junk && op == WRITE_INSERT && r.tid.is_none();
        // D655 Row C: a MERGE action under its own redo (merge/epq.rs), or its NOT MATCHED INSERT (never rechecks)
        let redo = match &arm { Arm::MergeEpq(_, l) => Some(*l), _ => None };
        if !cells_ok || matches!(arm, Arm::Conflict(_)) && self.conflict.is_none() || !tid_ok || self.epq && rerun.is_none() && redo.is_none() && op != WRITE_INSERT {
            crate::refuse!(self.node, Reason::Bug { payload: "a row op called with cells, a tid or a conflict arm its declaration does not have" });
        }
        let (x, l_cold) = (env.x, b.create_block(&[Type::I64]));
        b.mark_cold(l_cold);
        let uns = b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE));
        // the position: the driving source's locator, the junk ctid's ItemPointerData (bi_hi, bi_lo, ip_posid), or none
        // (INSERT); a driving source without a locator is M1's DELETE rule (a Result under a constant-false gate: the row
        // never comes), and a NULL junk ctid (PG: "ctid is NULL") leaves unsupported
        let rsite = match (self.loc, env.loc, r.tid) {
            // mergecmd RIGHT_OWN (held-out subq-0184): a MERGE NOT MATCHED INSERT under the driving declaration (a RIGHT merge
            // join whose outer is the target's own scan: a fill row has no outer row, and at the outer's end no locator)
            // writes with an empty position, as the junk-ctid declaration's INSERT arm below
            (LocFrom::Driving, _, _) if driving_insert(self.loc, op) => {
                self.empty_pos(b, env);
                None
            }
            (LocFrom::Driving, Some(l), _) => {
                store_loc(b, env, self.site.get(), self.relid, &l);
                env.site
            }
            (LocFrom::JunkCtid { site }, _, Some((d, valid))) => {
                let l_tid = b.create_block(&[]);
                b.cond_br(valid, (l_tid, &[]), (l_cold, &[uns]));
                b.switch_to(l_tid);
                let p = b.conv(ConvOp::IntToPtr, Type::Ptr, d);
                let (hi, lo, off) = (b.load_zext(Type::I16, p, 0), b.load_zext(Type::I16, p, 2), b.load_zext(Type::I16, p, 4));
                let sixteen = b.iconst(Type::I64, 16);
                let hi = b.int_bin(IntBinOp::Shl, hi, sixteen);
                let block = b.int_bin(IntBinOp::Or, hi, lo);
                store_pos(b, env, self.site.get(), self.relid, block, off);
                // G2(c): the row's own relation (the multi-target host's leaf) over the declared one
                if let Some((rel, _)) = r.rel {
                    let rel = b.conv(ConvOp::Trunc, Type::I32, rel);
                    b.store(env.ctx, site_off(self.site.get()) as i32 + site::POS_REL as i32, rel);
                }
                Some(site)
            }
            (LocFrom::None, _, _) => None,
            (LocFrom::JunkCtid { .. }, _, None) => {
                self.empty_pos(b, env);
                None
            }
            _ => {
                let (t, l_on) = (b.bconst(true), b.create_block(&[]));
                b.cond_br(t, (l_cold, &[uns]), (l_on, &[]));
                b.switch_to(l_on);
                None
            }
        };
        self.store_cells(b, env, r.cells);
        crate::rerun::epq::marks::store(b, env, row, self.node, &self.marks);
        let st = host_op(b, env, self.site.get(), op);
        let (st, l_st) = super::rowop_epq::arms(b, st, rerun.is_some());
        let st = crate::ops::modifytable::rowop_sub::serve(b, env, self.site.get(), op, st, (&self.fed, row), self.node);
        // below zero the host's error; above the op's last status (CONFLICT only with a conflict site) unsupported
        let top = b.iconst(Type::I64, if self.conflict.is_some() { WRITE_CONFLICT } else { WRITE_RECHECK });
        let (odd, neg) = (b.icmp(IntCond::Sgt, st, top), b.icmp(IntCond::Slt, st, env.zero));
        // W6: a MERGE's NOT_MATCHED (its target row concurrently deleted) is no odd status: its NOT MATCHED actions, else a skip
        let merge = matches!(arm, Arm::Merge(_) | Arm::MergeEpq(..)).then(|| b.iconst(Type::I64, WRITE_NOT_MATCHED));
        let odd = merge.map_or(odd, |nm| { let ok = b.icmp(IntCond::Ne, st, nm); b.int_bin(IntBinOp::And, odd, ok) });
        let bad = b.int_bin(IntBinOp::Or, neg, odd);
        let err = b.select(neg, st, uns);
        let types = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let (l_ok, l_done, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&types[..]));
        b.cond_br(bad, (l_cold, &[err]), (l_ok, &[]));
        b.switch_to(l_cold);
        let e = b.block_params(l_cold)[0];
        stop_at(b, &x, row, e);
        b.switch_to(l_ok);
        let dn = b.iconst(Type::I64, WRITE_DONE);
        let is_done = b.icmp(IntCond::Eq, st, dn);
        let l_not = b.create_block(&[]);
        b.cond_br(is_done, (l_done, &[]), (l_not, &[]));
        b.switch_to(l_done);
        let c = done(b, env, row);
        b.br(l_join, &c.params());
        // RECHECK: a stop after the row, the EvalPlanQual recheck naming the target's site (INSERT has none: unsupported)
        b.switch_to(l_not);
        let rc = b.iconst(Type::I64, WRITE_RECHECK);
        let is_rc = b.icmp(IntCond::Eq, st, rc);
        let (l_rc, l_rest) = (b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_rc);
        b.cond_br(is_rc, (l_rc, &[]), (l_rest, &[]));
        b.switch_to(l_rc);
        // G2(c): or the row's own relation's feed site, per row (`RowOpRow::rel`)
        let rs = match (r.rel, rsite) {
            (Some((_, code)), Some(_)) => code,
            _ => rsite.map_or(uns, |s| b.iconst(Type::I64, recheck(s))),
        };
        match (rerun.zip(l_st), redo) {
            (Some(((e, cells), l)), _) => super::rowop_epq::retry(b, env, row, (self.site.get(), self.node, op), e, cells, &|b, env, c| self.store_cells(b, env, c), l, l_join, &x),
            // D655 Row C: the MERGE's redo loop (merge/epq.rs)
            (None, Some(l)) => b.br(l, &[]),
            (None, None) => b.br(x.l_stop_in, &[row.carry.out, row.next, rs]),
        }
        // CONFLICT with its arm, a MERGE's NOT_MATCHED with its NOT MATCHED actions (W6), else SKIPPED: the carry as it came
        b.switch_to(l_rest);
        match arm {
            Arm::Conflict(on) | Arm::Merge(Some(on)) | Arm::MergeEpq(Some(on), _) => {
                let cf = merge.unwrap_or_else(|| b.iconst(Type::I64, WRITE_CONFLICT));
                let is_cf = b.icmp(IntCond::Eq, st, cf);
                let l_cf = b.create_block(&[]);
                b.cond_br(is_cf, (l_cf, &[]), (l_join, &row.carry.params()));
                b.switch_to(l_cf);
                if merge.is_none() {
                    instr::count_one(b, env.ctx, self.write_node, instr::Ctr::Tuples2);
                }
                let c = on(b, env, row);
                b.br(l_join, &c.params());
            }
            Arm::None | Arm::Merge(None) | Arm::MergeEpq(None, _) | Arm::Epq(..) => {
                if self.nothing && op == WRITE_INSERT {
                    instr::count_one(b, env.ctx, self.write_node, instr::Ctr::Tuples2);
                }
                b.br(l_join, &row.carry.params())
            }
        }
        b.switch_to(l_join);
        Carry::from_params(b.block_params(l_join), env.mcx)
    }

    fn cells<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, conflict: bool) -> PgVec<'e, (Value, Value)> {
        let at = match (conflict, &self.conflict) {
            (false, _) => site_off(self.site.get()),
            (true, Some(c)) if c.get() == self.site.get() + 1 => site_off(c.get()),
            _ => crate::refuse!(self.node, Reason::Bug { payload: "a conflict site read without one, or not the write site's successor" }),
        } as i32;
        let mut out = self.load_cells(b, env, at, self.natts);
        match &self.sys {
            None => {}
            // D774: the written row's system columns after its cells (write.returning's continuation lays them out); V4R-501:
            // an ON CONFLICT's (the inserted row's, DO UPDATE's new version's) after the conflict and old sites
            Some(y) if y.get() == self.site.get() + 1 + self.conflict.is_some() as usize + self.old.is_some() as usize => {
                out.extend_in(self.load_cells(b, env, site_off(y.get()) as i32, SYS_CELLS));
            }
            _ => crate::refuse!(self.node, Reason::Bug { payload: "a sys site not after the conflict and old sites" }),
        }
        out
    }

    fn old_cells<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> PgVec<'e, (Value, Value)> {
        let at = match &self.old {
            // no RETURNING OLD (the plan fetches nothing): no old cells
            None => return PgVec::new_in(env.mcx),
            Some(o) if o.get() == self.site.get() + 1 + self.conflict.is_some() as usize => site_off(o.get()) as i32,
            _ => crate::refuse!(self.node, Reason::Bug { payload: "an old site that is not the updated site's successor" }),
        };
        self.load_cells(b, env, at, self.old_n)
    }

    fn site(&self) -> usize {
        self.site.get()
    }

    fn epq_in_ir(&self) -> bool {
        self.epq
    }

    fn write_node(&self) -> i32 {
        self.write_node
    }
}

impl RowOpStep<'_> {
    /// The `n` cells of the site at ctx offset `at`: `(datum, isnull)` in attno order.
    fn load_cells<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, at: i32, n: u16) -> PgVec<'e, (Value, Value)> {
        let (vp, np) = (b.load(Type::Ptr, env.ctx, at + site::VALUES as i32), b.load(Type::Ptr, env.ctx, at + site::ISNULL as i32));
        let mut out = mcx::reserved_in(n as usize, env.mcx);
        for a in 0..n as i32 {
            let (v, n) = (b.load(Type::I64, vp, a * 8), b.load_zext(Type::I8, np, a));
            out.push_in((v, b.icmp(IntCond::Ne, n, env.zero)));
        }
        out
    }

    /// No target row (INSERT): the write site's position flags, relation and block zeroed.
    fn empty_pos(&self, b: &mut FunctionBuilder<'_>, env: &Env) {
        let (s, z) = (site_off(self.site.get()) as i32, b.iconst(Type::I32, 0));
        for f in [site::POS_FLAGS, site::POS_REL, site::POS_BLOCK] {
            b.store(env.ctx, s + f as i32, z);
        }
    }

    /// The target's cells into the write site by attno: a NULL (invalid) cell as a zero word with its null byte set.
    fn store_cells(&self, b: &mut FunctionBuilder<'_>, env: &Env, cells: &[(Value, Value)]) {
        if cells.is_empty() {
            return;
        }
        let at = site_off(self.site.get()) as i32;
        let (vp, np) = (b.load(Type::Ptr, env.ctx, at + site::VALUES as i32), b.load(Type::Ptr, env.ctx, at + site::ISNULL as i32));
        let (z8, o8) = (b.iconst(Type::I8, 0), b.iconst(Type::I8, 1));
        for (a, &(d, valid)) in cells.iter().enumerate() {
            let v = b.select(valid, d, env.zero);
            b.store(vp, a as i32 * 8, v);
            let n = b.select(valid, z8, o8);
            b.store(np, a as i32, n);
        }
    }
}

/// RIGHT_OWN (held-out subq-0184): `call_op`'s first arm, an INSERT under the
/// driving declaration, writes with an empty position (a MERGE NOT MATCHED INSERT whose fill row has no outer row, and at
/// the outer's end no locator), as the junk-ctid declaration's INSERT arm does. Only that pair: the driving declaration's
/// UPDATE / DELETE keep the locator arm, and the junk-ctid and no-target declarations keep their own.
fn driving_insert(loc: LocFrom, op: u32) -> bool {
    loc == LocFrom::Driving && op == WRITE_INSERT
}

#[cfg(test)]
mod tests {
    use gravity_abi::program::{WRITE_DELETE, WRITE_INSERT, WRITE_UPDATE};

    use super::{driving_insert, LocFrom};

    /// The Driving-INSERT arm matches an INSERT under the driving declaration and nothing else.
    #[test]
    fn the_driving_insert_arm_matches_only_an_insert_under_the_driving_declaration() {
        assert!(driving_insert(LocFrom::Driving, WRITE_INSERT));
        for op in [WRITE_UPDATE, WRITE_DELETE, 0, 4] {
            assert!(!driving_insert(LocFrom::Driving, op), "the driving declaration's op {op} keeps the locator arm");
        }
        for loc in [LocFrom::JunkCtid { site: 3 }, LocFrom::None] {
            for op in [WRITE_INSERT, WRITE_UPDATE, WRITE_DELETE] {
                assert!(!driving_insert(loc, op), "{op} under a non-driving declaration keeps its own arm");
            }
        }
    }
}
