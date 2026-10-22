//! join.inner's probe walk (M7 L0-b, move-only; M7-plan §1.3): the consumed build's header (`Hv`), the first record of
//! a row's image in the build's form (the KeyHash directory head with its bloom tags, `DENSE` / `DBITS`), the joined
//! row, a record handed on under the join qual with the offset word, join.mult's weighted row, the
//! single-row body (V4R-27), D688's re-entry test and a record's address. Moved out of join/probe_inner.rs, which keeps
//! the step (`make`, `make_swapped`, `Probe` and its `StepMech`). Declared in registry/m1.d (M1's tree: the helpers it
//! reads are M1's). Owner M7-PROBE (files-extra). Budget 240 lines.
use gravity_expr::emit::hash::{emit_home, emit_key_hash, Bits};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::jointype::JoinType;

use crate::cell::{Cell, Nullable};
use crate::table::keyimg;
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::row::{Carry, RowVals};

use crate::ops::hashjoin::build::{A_DENSE, A_TAGS, DBITS, HASH, IDX};
use crate::ops::hashjoin::probe::Probe;

/// The consumed build's header, read at each window.
#[derive(Clone, Copy)]
pub(crate) struct Hv {
    pub form: Value,
    pub dir: Value,
    pub bits: Value,
    pub lo: Value,
    pub span: Value,
    pub slots: Value,
    /// The records' address and the region's (`I64`).
    pub recs: Value,
    pub base: Value,
    /// The records (`I64`).
    pub n: Value,
    /// `DBITS`'s value and NULL planes (dense_bits.rs `planes`), when the probe may meet that form.
    pub planes: Option<(Value, Value)>,
}

/// A hit's payload: a record's address (`Ptr`) and link, or its words and NULL bits as values (the single-row body, V4R-156).
pub(crate) enum Rec<'a> {
    At(Value, Value),
    /// The payload words, NULL bits and the live Fp128 words, `lo, hi` each (join.payload_fp, `Shape::fp_live`'s order).
    Words(&'a [Value], &'a [Value], &'a [Value]),
}

impl Probe<'_> {
    pub(crate) fn dense(&self) -> bool {
        self.sh.range.get() && self.sh.allow & A_DENSE != 0
    }

    /// The first record of the image's chain in the KeyHash directory (0: none), the bloom tags tested first. D3194: in
    /// the frame's filtered body (`Env::rowfilter` names this step), the build's Bloom words before the head: a row whose
    /// bits are not all set has no record, as a head without its chain (rowfilter.rs `test`); not again for a block's rows
    /// its block test passed (`Env::prefiltered`). D3168: a row of a staged block reads the first record its block's
    /// stages found (`Env::batch_row`, probe_stage.rs); any other row of the staged body (`Env::batch`) tests the words
    /// only while they are armed, which that body's guard (the directory's size) does not imply.
    fn hash_first(&self, b: &mut FunctionBuilder<'_>, env: &Env, hv: &Hv, img: &keyimg::ImageWords) -> Value {
        if let Some(r) = env.batch_row.filter(|r| r.offset == self.offset) {
            let at = b.ptr_add(r.firsts, r.t, 8, 0);
            return b.load(Type::I64, at, 0);
        }
        let h = emit_key_hash(b, &img.words);
        let staged = env.batch.is_some() && env.batch == env.step;
        let tests = env.step.is_some() && (env.rowfilter == env.step || staged) && !env.prefiltered;
        let rf = self.rf.zip(self.rfl.get()).filter(|_| tests);
        let l_out = rf.map(|((w, _), f)| {
            let (l_dir, l_out) = (b.create_block(&[]), b.create_block(&[Type::I64]));
            if staged {
                let l_test = b.create_block(&[]);
                b.cond_br(f.armed, (l_test, &[]), (l_dir, &[]));
                b.switch_to(l_test);
            }
            crate::ops::hashjoin::strategies::rowfilter::test(b, env, &f, h, w, (l_dir, l_out));
            b.switch_to(l_dir);
            l_out
        });
        let hp = head_at(b, hv, h);
        let head = b.load(Type::I64, hp, 0);
        let p = self.head_first(b, env, hv, h, head);
        match l_out {
            Some(l) => {
                b.br(l, &[p]);
                b.switch_to(l);
                b.block_params(l)[0]
            }
            None => p,
        }
    }

    /// The first record (`I64`, 0: none) of directory head word `head` for key hash `h`: the head's index, its bloom tags
    /// tested when the build lays them.
    pub(crate) fn head_first(&self, b: &mut FunctionBuilder<'_>, env: &Env, hv: &Hv, h: Value, head: Value) -> Value {
        let idx = b.iconst(Type::I64, IDX as i64);
        let i1 = b.int_bin(IntBinOp::And, head, idx);
        let none = b.icmp(IntCond::Eq, i1, env.zero);
        let none = match self.sh.allow & A_TAGS {
            0 => none,
            _ => {
                let ok = crate::ops::hashjoin::strategies::bloomtag::admits(b, head, h);
                let t = b.bconst(true);
                let miss = b.int_bin(IntBinOp::Xor, ok, t);
                b.int_bin(IntBinOp::Or, none, miss)
            }
        };
        let p = rec_at(b, env, hv, i1, self.sh.rec.get());
        b.select(none, env.zero, p)
    }

    /// The joined row: the probe row's cells, then each payload column's record cell (`Env::inner` maps INNER_VAR to them),
    /// and join.payload_fp's links: a code column's word with the Fp128 words its record carries (`Env::fps`).
    fn joined<'r>(&self, b: &mut FunctionBuilder<'_>, hv: &Hv, row: &RowVals<'r>, rec: &Rec<'_>, carry: &Carry) -> (RowVals<'r>, crate::lx::FpLinks) {
        let m = *row.vals.allocator();
        let mut r = RowVals { row: crate::cell::Row { cells: mcx::to_vec_in(&row.row.cells[..], m) }, vals: mcx::to_vec_in(&row.vals[..], m), cols: mcx::to_vec_in(&row.cols[..], m), carry: carry.copy_in(m), next: row.at, at: row.at };
        let (one, zero) = (b.iconst(Type::I64, 1), b.iconst(Type::I64, 0));
        let mut links = crate::lx::FpLinks::NONE;
        for (i, c) in self.sh.kept.borrow().iter().enumerate() {
            let mut w = match *rec {
                Rec::At(r, _) => b.load(Type::I64, r, self.sh.pay_off(i)),
                Rec::Words(w, _, _) => w[i],
            };
            if self.sh.is_rel(i) {
                w = b.int_bin(IntBinOp::Add, w, hv.base);
            }
            if let Some(j) = self.sh.fp_at(i) {
                let fp = match *rec {
                    Rec::At(r, _) => [b.load(Type::I64, r, self.sh.fp_off(j)), b.load(Type::I64, r, self.sh.fp_off(j) + 8)],
                    Rec::Words(_, _, f) => [f[2 * j], f[2 * j + 1]],
                };
                links.add(w, fp);
            }
            let bit = match *rec {
                Rec::At(p, link) => {
                    let (off, at) = self.sh.null_at(i);
                    let word = if off == self.sh.link_off() { link } else { b.load(Type::I64, p, off) };
                    let s = b.iconst(Type::I64, at as i64);
                    let bits = b.int_bin(IntBinOp::Ushr, word, s);
                    b.int_bin(IntBinOp::And, bits, one)
                }
                Rec::Words(_, n, _) => n[i],
            };
            let null = b.icmp(IntCond::Ne, bit, zero);
            let life = crate::ops::hashjoin::build_side::home_of(c, self.life);
            r.row.cells.push_in(Cell::of_record(c.rep, c.typid, c.num, life, i as u16, Nullable::Maybe));
            r.vals.push_in((w, Some(null)));
        }
        (r, links)
    }
}

impl Probe<'_> {
    /// The first candidate record of the row's image (`I64`, 0: none) in the build's form, and whether the form's
    /// candidates are all the image's own records (every form but HASH). `bits`: the bits form is read here too.
    pub(crate) fn first(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, hv: &Hv, img: &keyimg::ImageWords, bits: bool) -> (Value, Value) {
        let l_f = b.create_block(&[Type::I64]);
        let rec = self.sh.rec.get();
        let hash = b.iconst(Type::I64, HASH as i64);
        if self.dense() {
            if bits && crate::ops::hashjoin::strategies::dense_bits::may(&self.sh) {
                let db = b.iconst(Type::I64, DBITS as i64);
                let is_b = b.icmp(IntCond::Eq, hv.form, db);
                let (l_b, l_nb) = (b.create_block(&[]), b.create_block(&[]));
                b.cond_br(is_b, (l_b, &[]), (l_nb, &[]));
                b.switch_to(l_b);
                let p = crate::ops::hashjoin::strategies::dense_bits::first(b, env, hv, img.words.words[0], &self.sh);
                b.br(l_f, &[p]);
                b.switch_to(l_nb);
            }
            let is_h = b.icmp(IntCond::Eq, hv.form, hash);
            let (l_h, l_d) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(is_h, (l_h, &[]), (l_d, &[]));
            b.switch_to(l_d);
            let p = crate::ops::hashjoin::strategies::dense::first(b, env, hv, img.words.words[0], self.sh.words.get() as i64, rec);
            b.br(l_f, &[p]);
            b.switch_to(l_h);
        }
        let p = self.hash_first(b, env, hv, img);
        b.br(l_f, &[p]);
        b.switch_to(l_f);
        let exact = match self.dense() {
            true => b.icmp(IntCond::Ne, hv.form, hash),
            false => b.bconst(false),
        };
        (b.block_params(l_f)[0], exact)
    }

    /// Record `rec` (its `link`) joined to the row and handed on with `carry`: the join qual first (a failing row goes to
    /// `fail`), then the offset word at `m1` (the matches so far, this one too) while `next` runs.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn hand_on<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, hv: &Hv, row: &RowVals, (rec, carry): (Rec<'_>, &Carry), m1: Option<Value>, fail: (Block, &[Value]), next: Next<'_, 'e>) -> Carry<'e> {
        let (mut row2, links) = self.joined(b, hv, row, &rec, carry);
        // join.payload_fp: the links hold on this path only, so each path past the probe sees the same ones (the miss's,
        // probe_fill.rs `miss`) and a step made again on each keeps one cell form (a build end keeps its last making's)
        let fps = env.fps;
        env.fps.merge(&links);
        if m1.is_none() {
            row2.next = row.next; // the single-row body: a stop after the row resumes past it
        }
        let base = row.row.cells.len();
        let width = self.sh.payload.iter().copied().max().unwrap_or(0).max(0) as usize;
        let lay = |map: &mut [Option<u16>]| {
            for (i, &a) in self.sh.payload.iter().enumerate() {
                map[a as usize - 1] = Some((base + i) as u16);
            }
        };
        // swapped: the record is the OUTER side (the row's columns), the arriving row the INNER side (`env.inner`)
        let (row2, map, pay) = match self.sh.swapped {
            true => {
                let mut cols = mcx::pg_vec![in env.mcx; None; width];
                lay(&mut cols[..]);
                (RowVals { cols, ..row2 }, mcx::to_vec_in(&row.cols[..], env.mcx), None)
            }
            false => {
                let mut map = mcx::pg_vec![in env.mcx; None; width];
                lay(&mut map[..]);
                // producerchain D: the record's payload row (its cells, values and INNER_VAR map) in reach of a write's EvalPlanQual
                // redo in the IR (epq_ir.rs `Redo::Probe`), by the probe's node, as nest.loop keeps its inner row
                let pay = (mcx::to_vec_in(&row2.row.cells[base..], env.mcx), mcx::to_vec_in(&row2.vals[base..], env.mcx), map.iter().map(|c| c.map(|k| k - base as u16)).collect_in(env.mcx), crate::frame::env::Filled::No);
                (row2, map, Some(pay))
            }
        };
        let saved = env.inner.replace(map);
        for (i, &q) in self.qual.iter().enumerate() {
            // D2003: PG's right semi join skips an inner tuple already matched once its hash clauses pass, before the join
            // qual (nodeHashjoin.c HJ_SCAN_BUCKET: JOIN_RIGHT_SEMI and HeapTupleHeaderHasMatch), so that qual never runs on it
            if let (true, JoinType::JOIN_RIGHT_SEMI, Rec::At(r, _)) = (i == self.confirmed, self.jt, &rec) {
                let marks = crate::ops::hashjoin::build::marks_base(b, hv.base);
                let (seen, l_new) = (crate::ops::hashjoin::build::marked(b, marks, hv.recs, *r, self.sh.rec.get()), b.create_block(&[]));
                b.cond_br(seen, fail, (l_new, &[]));
                b.switch_to(l_new);
            }
            let pass = crate::step::expr::pred(b, env, self.node, &row2, q);
            let l_yes = b.create_block(&[]);
            b.cond_br(pass, (l_yes, &[]), fail);
            b.switch_to(l_yes);
        }
        if let Some(m1) = m1 {
            env.store_word(b, self.offset, m1);
        }
        // M7 (R1-R3): INNER / LEFT hand the joined row on; SEMI too, once (`first_only` ends its walk at this record): PG's
        // single_match projects the outer row with its first matching inner tuple, so the inner columns read above the join
        // (a write's REFERENCE mark ctid or COPY mark row, CNM-40) are this record's payload; ANTI nothing (the match drops
        // the row: its walk ends and the offset word keeps the fill off)
        let held = pay.map(|p| env.redo_nests.push_in((self.node, p))).is_some();
        // D3870 BUILT-HELD: and the probe row, for a write whose target is this probe's build (epq_ir.rs `Redo::InnerOf`
        // over `epq_of_built_held`'s build redo), as nest.loop keeps its outer row
        let outer = (!self.sh.swapped).then(|| env.redo_outers.push_in((self.node, (mcx::to_vec_in(&row.row.cells[..], env.mcx), mcx::to_vec_in(&row.vals[..], env.mcx), mcx::to_vec_in(&row.cols[..], env.mcx), crate::frame::env::Filled::No)))).is_some();
        let c2 = match self.jt {
            JoinType::JOIN_ANTI => carry.copy_in(env.mcx),
            // M7-DRAIN (V4R-378, M7-PROBE's Owner-Ack): the right family marks each record a row passed the join qual on
            // (a marking build lays only the KeyHash form: the record has its address); RIGHT / FULL hand the joined row on,
            // RIGHT SEMI / RIGHT ANTI nothing (the drain sends the build's marked / unmarked records)
            JoinType::JOIN_RIGHT | JoinType::JOIN_FULL | JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI => {
                let Rec::At(r, _) = rec else { crate::refuse!(self.node, crate::admit::refusal::Reason::Bug { payload: "a marking probe's record without its address" }) };
                let marks = crate::ops::hashjoin::build::marks_base(b, hv.base);
                // lockrows lk-0100 (probe.rs `semi_hands`): only the first match hands the record on (HJ_SCAN_BUCKET's
                // HeapTupleHeaderHasMatch skip, also with no join qual)
                if self.semi_hands {
                    let (seen, l_new) = (crate::ops::hashjoin::build::marked(b, marks, hv.recs, r, self.sh.rec.get()), b.create_block(&[]));
                    b.cond_br(seen, fail, (l_new, &[]));
                    b.switch_to(l_new);
                }
                crate::ops::hashjoin::build::mark(b, marks, hv.recs, r, self.sh.rec.get());
                if matches!(self.jt, JoinType::JOIN_RIGHT | JoinType::JOIN_FULL) || self.semi_hands { next(b, env, &row2) } else { carry.copy_in(env.mcx) }
            }
            _ => next(b, env, &row2),
        };
        if outer {
            env.redo_outers.pop();
        }
        if held {
            env.redo_nests.pop();
        }
        env.inner = saved;
        env.fps = fps;
        c2
    }

    /// join.mult: the row once, `Env::mult` times its key's count. None: an INNER probe's row ends here; a LEFT probe's
    /// (AGA-31) goes on once through the fill's miss path, its payload NULL, as the walk's end sends it (`kept`: the memo
    /// set aside before the keys, which no read on the hit path dominates).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn lower_mult<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, hv: &Hv, img: &keyimg::ImageWords, row: &RowVals, next: Next<'_, 'e>, (l_join, l_miss): (Block, Block), kept: Option<crate::lx::SetAside<'e>>) -> Carry<'e> {
        let (first, exact) = self.first(b, env, hv, img, true);
        let w = crate::ops::hashjoin::strategies::multiplicity::weight(b, env, hv, img, first, exact, &self.sh);
        let ctypes: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        // D3553 (B): a LEFT probe lowers its rest once (probe_share.rs); its payload is empty, so are its kept cells
        if self.share && self.sh.kept.borrow().is_empty() {
            return self.lower_mult_shared(b, env, (w, &ctypes), row, next, (l_join, l_miss), kept);
        }
        let none = b.icmp(IntCond::Eq, w, env.zero);
        let l_none = if self.fill.is_some() { b.create_block(&ctypes) } else { l_join };
        let l_go = b.create_block(&[]);
        b.cond_br(none, (l_none, &row.carry.params()), (l_go, &[]));
        b.switch_to(l_go);
        let m = match env.mult {
            Some(m0) => b.int_bin(IntBinOp::Mul, m0, w),
            None => w,
        };
        let saved = env.mult.replace(m);
        let c = next(b, env, row);
        env.mult = saved;
        b.br(l_join, &c.params());
        if self.fill.is_some() {
            b.switch_to(l_none);
            if let Some(m) = kept {
                env.memo.restore(m);
            }
            let ce = Carry::from_params(b.block_params(l_none), env.mcx);
            let ce = crate::ops::hashjoin::probe_fill::miss(b, env, (self.fill.map(|f| (f, self.offset, &*self.sh, self.life)), self.node), (ce, l_miss), row, next);
            env.store_word(b, self.offset, env.zero);
            b.br(l_join, &ce.params());
        }
        b.switch_to(l_join);
        Carry::from_params(b.block_params(l_join), env.mcx)
    }
}

impl Probe<'_> {
    /// V4R-27, the single-row body (`single_guard`: `DBITS` or `DENSE`): the key's one record or none, the join qual, the
    /// row handed on once with the cursor past it (no chain, no offset word, no carry through a walk), as a filter.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn lower_single<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, hv: &Hv, img: &keyimg::ImageWords, row: &RowVals, next: Next<'_, 'e>, l_join: Block) -> Carry<'e> {
        let h = crate::ops::hashjoin::strategies::dense::first_unique(b, env, hv, img.words.words[0], &self.sh, self.sh.kept.borrow().len());
        let (none, l_hit) = (b.icmp(IntCond::Eq, h.p, env.zero), b.create_block(&[]));
        b.cond_br(none, (l_join, &row.carry.params()), (l_hit, &[]));
        b.switch_to(l_hit);
        let (memo, cp) = (env.memo.set_aside(), row.carry.params());
        let c = self.hand_on(b, env, hv, row, (Rec::Words(&h.pay, &h.null, &h.pay[self.sh.kept.borrow().len()..]), &row.carry), None, (l_join, &cp), next);
        env.memo.restore(memo);
        b.br(l_join, &c.params());
        b.switch_to(l_join);
        Carry::from_params(b.block_params(l_join), env.mcx)
    }
}


/// D688: a later expanding step's offset word is set (it stopped inside the row's last match: re-enter it); `None` without one.
pub(crate) fn later_set(b: &mut FunctionBuilder<'_>, env: &Env) -> Option<Value> {
    let mut set = None;
    for &(_, o) in env.expands.iter().filter(|e| env.step.is_some_and(|j| e.0 > j)) {
        let w = env.load_word(b, o);
        set = Some(set.map_or(w, |a| b.int_bin(IntBinOp::Or, a, w)));
    }
    Some(b.icmp(IntCond::Ne, set?, env.zero))
}


/// The address of key hash `h`'s KeyHash directory head.
pub(crate) fn head_at(b: &mut FunctionBuilder<'_>, hv: &Hv, h: Value) -> Value {
    let s = emit_home(b, h, Bits::Dyn(hv.bits), Bits::Const(0));
    b.ptr_add(hv.dir, s, 8, 0)
}

/// The address (`I64`) of record `i1 - 1`; garbage when `i1` is 0 (the caller selects it away).
pub(crate) fn rec_at(b: &mut FunctionBuilder<'_>, env: &Env, hv: &Hv, i1: Value, rec: u32) -> Value {
    let i0 = b.int_bin(IntBinOp::Sub, i1, env.one);
    let r = b.iconst(Type::I64, rec as i64);
    let off = b.int_bin(IntBinOp::Mul, i0, r);
    b.int_bin(IntBinOp::Add, hv.recs, off)
}
