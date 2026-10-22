//! D3496: an UPDATE's or DELETE's RETURNING over a flattened UNION ALL subquery whose COPY mark's recheck scans the members
//! again (update_built.rs `copied_settled`'s settled union, marks.rs's member rescan). PostgreSQL 18.6 projects the list
//! over the ORIGINAL plan row (ExecProcessReturning's `context->planSlot`), never EvalPlanQual's re-joined one, so on the
//! recheck node the list's union columns are the mark's stored `wholerow{rowmarkId}` record's fields (marks.rs keeps it in
//! a copy site of its own, `POS_COPY` there on the recheck node only); elsewhere the row's own.
use std::cell::Cell as SiteCell;
use std::rc::Rc;

use gravity_abi::ctx::site_off;
use gravity_abi::site::site;
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_decompose::ops::shared::modifytable::WriteDesc;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, Type, Value};
use gravity_program::{ExprLife, ExprOut, OutCol};
use mcx::{CollectIn, GrowIn, PgVec};

use crate::admit::refusal::Reason;
use crate::cell::{Cell, Nullable, Row};
use crate::face::kinds::Face;
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::row::RowVals;
use crate::rerun::epq::copy_row::{copying, fields_of};
use crate::rerun::epq::marks::CopySites;

/// The mark's copy site and each list column's resno, record field, plain form and type facts.
pub(crate) struct Stored<'g> {
    site: Rc<SiteCell<usize>>,
    cols: PgVec<'g, (i16, i16, ExprOut, TypeFacts)>,
}

/// The stored row the RETURNING list of pipeline `k`'s write `w` reads on the recheck, made after the row op whose marks
/// made the copy site; `None` when the recheck keeps the plan row's union columns.
pub(crate) fn of<'g>(px: &Px<'g, '_>, k: usize, w: &WriteDesc<'_>, node: i32) -> Option<Stored<'g>> {
    let gx = px.gx;
    let (rti, cols) = super::update_built::ret_record(gx, w)?;
    let sites = px.ext::<CopySites>(Face::End);
    if !sites.is_some_and(|c| c.3.contains(&rti)) && super::update_built::copied_settled(gx, k, w).is_none() {
        return None;
    }
    let Some(site) = sites.and_then(|c| c.0.iter().find(|e| e.0 == rti)).map(|e| e.1.clone()) else {
        crate::refuse!(node, Reason::Bug { payload: "a RETURNING over a union's stored record without its copy site" })
    };
    let mut out = mcx::reserved_in(cols.len(), px.mcx());
    for (resno, field, typ) in cols {
        let Some(tf) = typefacts::facts_of(gx.facts, typ) else { crate::refuse!(node, Reason::Bug { payload: "a RETURNING union column whose type the catalog does not know" }) };
        out.push_in((resno, field, ExprOut { rep: OutCol::new("", typ, -1, tf.typlen, tf.byval).rep, typid: typ, life: ExprLife::Row }, tf));
    }
    Some(Stored { site, cols: out })
}

impl Stored<'_> {
    /// `row` with each list column of the union a plain datum cell, appended: the stored record's field on the recheck
    /// node, else the row's own value.
    pub(crate) fn row<'r>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals<'r>) -> RowVals<'r> {
        let mut own: mcx::PgVec<'_, Value> = mcx::reserved_in(2 * self.cols.len(), env.mcx);
        let mut cx = env.lx(b, node);
        for (resno, _, _, tf) in self.cols.iter() {
            let Some(c) = row.col(*resno) else { crate::refuse!(node, Reason::Bug { payload: "a RETURNING union column the row does not carry" }) };
            let (d, null) = crate::rep::datum(&mut cx, c, tf);
            own.extend_in([d, null]);
        }
        env.memo = cx.memo;
        let types: mcx::PgVec<'_, Type> = self.cols.iter().flat_map(|_| [Type::I64, Type::Bool]).collect_in(env.mcx);
        let (l_rec, l_join) = (b.create_block(&[]), b.create_block(&types[..]));
        let on = copying(b, env, self.site.get());
        b.cond_br(on, (l_rec, &[]), (l_join, &own[..]));
        b.switch_to(l_rec);
        let img = b.load_unaligned(Type::I64, env.ctx, site_off(self.site.get()) as i32 + site::POS_REL as i32);
        let fields: mcx::PgVec<'_, (i16, u32)> = self.cols.iter().map(|c| (c.1, c.2.typid)).collect_in(env.mcx);
        let got = fields_of(b, env, img, &fields[..]);
        let t = b.bconst(true);
        let mut rec: mcx::PgVec<'_, Value> = mcx::reserved_in(2 * got.len(), env.mcx);
        for (v, ok) in got {
            let null = b.int_bin(IntBinOp::Xor, ok, t);
            rec.extend_in([v, null]);
        }
        b.br(l_join, &rec[..]);
        b.switch_to(l_join);
        let ps = b.block_params(l_join);
        let (mut cells, mut vals, mut cols) = (row.row.cells.clone(), row.vals.clone(), row.cols.clone());
        for (i, (resno, _, out, _)) in self.cols.iter().enumerate() {
            let at = cells.len();
            cells.push_in(Cell::of_expr(*out, at as u16, at as u16, Nullable::Maybe));
            vals.push_in((ps[2 * i], Some(ps[2 * i + 1])));
            if let Some(c) = usize::try_from(*resno).ok().and_then(|r| r.checked_sub(1)).and_then(|r| cols.get_mut(r)) {
                *c = Some(at as u16);
            }
        }
        env.memo.clear();
        RowVals { row: Row { cells }, vals, cols, carry: row.carry.copy_in(*row.vals.allocator()), next: row.next, at: row.at }
    }
}
