//! D3477 (settled MERGE): a MERGE whose source is a flattened UNION ALL subquery probing the hashed target, its actions
//! WHEN MATCHED only (ops/modifytable/update_built.rs `settled_merge`). The subquery's COPY mark names a relation no scan
//! carries, so PostgreSQL 18.6's EvalPlanQual scans the union's members again, whole, and joins them with the target's
//! newest version; its first joined row makes ExecMergeMatched call the row MATCHED (none: nothing, as no NOT MATCHED
//! action is there), and the actions then run over the newest version with the ORIGINAL source row (lmerge_matched again
//! over `context->planSlot`), never the re-run one. Here the recheck reruns the feeds (marks.rs `Marks::rescan`) and the
//! target's producer at the newest version; the end takes the recheck's first joined row alone (a per-open word: the
//! recheck node opens the pipeline afresh) and reads its subquery columns from the mark's stored `wholerow{rowmarkId}`
//! record (marks.rs stores it in a copy site of its own, held on the recheck node as `POS_COPY`), each by its field number
//! (decompose graph `copy_union_cols`). The statement's own rows act as any MERGE's. Helper file of write.merge: no rows
//! of its own.
use std::cell::Cell as SiteCell;
use std::rc::Rc;

use gravity_abi::ctx::site_off;
use gravity_abi::site::site;
use gravity_decompose::catalog::typefacts;
use gravity_decompose::ops::shared::modifytable::WriteDesc;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntBinOp, IntCond, Type};
use gravity_program::{ExprLife, ExprOut, OutCol};
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::plannodes::RowMarkType;

use crate::admit::refusal::Reason;
use crate::cell::{Cell, Nullable, Row};
use crate::face::kinds::Face;
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Px, WordKey};
use crate::frame::row::RowVals;

use super::lower::MergeEnd;

/// The recheck's stored source row and its first-row word (module doc).
pub(crate) struct Settled<'g> {
    /// The COPY mark's copy site (marks.rs `sites_in`'s settled MERGE leg).
    site: Rc<SiteCell<usize>>,
    /// Each child column of the subquery: its resno, the record's field holding it and the field's form.
    cols: PgVec<'g, (i16, i16, ExprOut)>,
    /// The per-open word set once the recheck's first joined row reached the actions.
    word: i32,
}

/// The end's open word index (`WordKey::Open`, `Face::End`) of the first-row flag.
const FIRST_WORD: u8 = 0;

/// The settled form's stored row for MERGE `w` at pipeline `k`, when update_built.rs `copied_settled` takes it, or D3641
/// when the recheck scans the union's members again under the target's probe (marks.rs's member-rescan leg, which names
/// the mark in `CopySites`): made after the row op, whose marks made the copy site; else `None`.
pub(super) fn of<'g>(px: &mut Px<'g, '_>, k: usize, w: &WriteDesc<'_>, node: i32) -> Option<Settled<'g>> {
    let gx = px.gx;
    let rescanned = |rti: u32| px.ext::<crate::rerun::epq::marks::CopySites>(Face::End).is_some_and(|c| c.3.contains(&rti));
    let leg = w.mark_descs.iter().any(|m| m.mark == RowMarkType::ROW_MARK_COPY && rescanned(m.rti));
    if w.merge.is_none() || crate::ops::modifytable::update_built::copied_settled(gx, k, w).is_none() && !leg {
        return None;
    }
    let union = |rti: u32| gx.graph.copy_unions.iter().any(|e| e.0 == rti);
    let Some(rti) = w.mark_descs.iter().find(|m| m.mark == RowMarkType::ROW_MARK_COPY && union(m.rti)).map(|m| m.rti) else {
        crate::refuse!(node, Reason::Bug { payload: "a settled MERGE without its union's COPY mark" })
    };
    let made = px.ext::<crate::rerun::epq::marks::CopySites>(Face::End).and_then(|c| c.0.iter().find(|e| e.0 == rti)).map(|e| e.1.clone());
    let Some(site) = made else { crate::refuse!(node, Reason::Bug { payload: "a settled MERGE's COPY mark without its copy site" }) };
    let mut cols = PgVec::new_in(px.mcx());
    for &(_, resno, field, typ) in gx.graph.copy_union_cols.iter().filter(|c| c.0 == rti) {
        let Some(tf) = typefacts::facts_of(gx.facts, typ) else { crate::refuse!(node, Reason::Bug { payload: "a settled MERGE's source column type the catalog does not know" }) };
        cols.push_in((resno, field, ExprOut { rep: OutCol::new("", typ, -1, tf.typlen, tf.byval).rep, typid: typ, life: ExprLife::Row }));
    }
    let word = px.frame_word(WordKey::Open { face: Face::End, k, j: u64::MAX, i: FIRST_WORD }) as i32 * 8;
    Some(Settled { site, cols, word })
}

impl Settled<'_> {
    pub(super) fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("write.merge settled first row", Base::Words, self.word, Type::I64, 0));
    }

    /// The actions over the joined row `at` (`arm` its target attributes): on the recheck node the first joined row's with
    /// the subquery's columns of the stored record, every later one's none; else the row's own. Every path ends at `l_join`.
    pub(super) fn lower<'e>(&self, end: &MergeEnd<'_>, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, (at, arm, l_join): (&RowVals, &RowVals, Block), next: &mut Option<Next<'_, 'e>>) {
        let on = crate::rerun::epq::copy_row::copying(b, env, self.site.get());
        let (l_rec, l_plain, l_first) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(on, (l_rec, &[]), (l_plain, &[]));
        b.switch_to(l_rec);
        let acted = env.load_word(b, self.word);
        let seen = b.icmp(IntCond::Ne, acted, env.zero);
        b.cond_br(seen, (l_join, &at.carry.params()), (l_first, &[]));
        b.switch_to(l_first);
        env.store_word(b, self.word, env.one);
        let rec = self.row(b, env, at);
        let rec_arm = RowVals { row: rec.row.clone(), vals: rec.vals.clone(), cols: arm.cols.clone(), carry: arm.carry.copy_in(env.mcx), next: arm.next, at: arm.at };
        env.inner = Some(mcx::to_vec_in(&rec.cols[..], env.mcx));
        env.memo.clear();
        end.acts(b, env, &end.actions, (&rec, &rec_arm, None, l_join), next, None);
        b.switch_to(l_plain);
        env.inner = Some(mcx::to_vec_in(&at.cols[..], env.mcx));
        env.memo.clear();
        end.acts(b, env, &end.actions, (at, arm, None, l_join), next, None);
    }

    /// `at` with each subquery column cell the stored record's field (a plain datum cell, appended).
    fn row<'r>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, at: &RowVals<'r>) -> RowVals<'r> {
        let img = b.load_unaligned(Type::I64, env.ctx, site_off(self.site.get()) as i32 + site::POS_REL as i32);
        let fields: mcx::PgVec<'_, (i16, u32)> = self.cols.iter().map(|c| (c.1, c.2.typid)).collect_in(env.mcx);
        let got = crate::rerun::epq::copy_row::fields_of(b, env, img, &fields[..]);
        let t = b.bconst(true);
        let (mut cells, mut vals, mut cols) = (at.row.cells.clone(), at.vals.clone(), at.cols.clone());
        for ((resno, _, out), (v, ok)) in self.cols.iter().zip(got) {
            let i = cells.len();
            let null = b.int_bin(IntBinOp::Xor, ok, t);
            cells.push_in(Cell::of_expr(*out, i as u16, i as u16, Nullable::Maybe));
            vals.push_in((v, Some(null)));
            if let Some(c) = usize::try_from(*resno).ok().and_then(|r| r.checked_sub(1)).and_then(|r| cols.get_mut(r)) {
                *c = Some(i as u16);
            }
        }
        RowVals { row: Row { cells }, vals, cols, carry: at.carry.copy_in(*at.vals.allocator()), next: at.next, at: at.at }
    }
}
