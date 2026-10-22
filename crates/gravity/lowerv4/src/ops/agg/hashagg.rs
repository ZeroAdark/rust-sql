//! Hashed grouping, kind `keyed.hashagg` (plan §3.1 the keyed face; H13 H06/H22 H10): a hashed group's end. Each row's key
//! cells go through keyimg.rs into the one image, the image through `emit_key_hash`; the record is the table's (TABLES,
//! face/keyed.rs `table`). A row folds into its record directly, or, when `run.collapse` is on and no code front takes the
//! table, into the open run's registers (`RunCarry`), folded into the record once at the run's close (the `RunFold`). The owner phase
//! merges decompose's record rekeyed to the image words (`keyed_coderep::image_layout`), its min / max(text) slots declared as
//! images by decompose. An `Fp128` key keeps a representative code (`part << 32 | code`, H10) in its
//! `MinCode` slot, written once when its record opens: the emit renders the group's text from it.
//!
//! `GroupRec` (the record as the keyed ends and the emit read it) lives in keyed/hashagg_rec.rs (a move-only split, main
//! 09-26), re-exported here for its readers. Budget 450 lines (files.tsv, +25 % gate). Owner M1-KEYED.
use gravity_abi::hash::Depth;
use gravity_decompose::ops::shared::agg::GroupForm;
use gravity_expr::emit::hash::emit_key_hash;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
use gravity_program::{Pipeline, Rep};
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};

use crate::face::end::EndMech;
use crate::face::fold::Acc;
use crate::table::face::{CountFrontMech, CountKey, CountShape, RunCarry, RunFold, TableDecl, TableMech};
use crate::table::keyimg::ImageWords;
use crate::face::kinds::KindId;
use crate::face::strategy::FRAME_CARRY;
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, Win};
use crate::frame::open::OpenWords;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::admit::refusal::Reason;

// moved to keyed/hashagg_rec.rs (a move-only split, main 09-26); re-exported for keyed_hashagg's readers
pub(crate) use crate::table::rec::{ex_val, AccAt, Agg, Arg, GroupRec, KeyVal};

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::KeyedHashagg, &EndMaker(make))], ..crate::ops::NONE };

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

/// The hash aggregate's end.
struct HashEnd<'g, 'p> {
    rec: GroupRec<'g, 'p>,
    t: TableDecl,
    table: &'static dyn TableMech,
    run: Option<&'static dyn RunCarry>,
    /// The end counts the rows into the sentinel (no aggregate does).
    count: bool,
    /// The code keys' `(attno, Fp128 table)`, their entries hinted ahead of the run close (`prefetch.fp`).
    pf: PgVec<'g, (i16, u8)>,
    /// Pass 2 of a two-pass form: only admitted groups fold (keyed_twopass).
    admit: Option<crate::ops::shared::twopass::Admit>,
    /// D3326 (`twopass::hoisted`): the admitted set's header read once a row loop, a run's key tested at its open.
    hoist: bool,
    /// The header words the row loop being lowered read (`whole`, its start), until its `window_end`.
    live: core::cell::Cell<Option<crate::ops::shared::twopass::AdmitLive>>,
    /// A twopass.having split's pass (its descriptor's slots).
    split: Option<crate::ops::shared::twopass::Split>,
    /// A derived key's code column, its image indexed by code.front_dense (keyed_emit_group `derived_col`).
    src: Option<i16>,
    /// The per-code front over the hashed table (keyed_codefront_percode).
    pc: Option<crate::dict::strategies::codefront_percode::Percode>,
    /// The constant-key window's fold (keyed_percode_const): a one-pass group's alone.
    pk: Option<crate::dict::strategies::percode_const::Plan<'g>>,
    /// M3 L0-d: a count-ordered top's owner (owner.count) and its bound (runs_topk_groups `count_top_of`; M1: none).
    top: Option<gravity_program::OwnerDesc>,
    /// V4R-177: a walked group's row skip (keyed_walk_skip), its bound word after the table's state.
    walk: Option<crate::ops::agg::walk_skip::Skip>,
    /// code.front_light (V4R-184): the count front over the hashed table, its shape and every key's column.
    light: Option<(&'static dyn CountFrontMech, CountShape<'g>, PgVec<'g, i16>)>,
    /// Road num.partial: the per-claim i64 partials' front of a plain hashed fold, its words after the table's.
    part: Option<crate::ops::agg::numeric::strategies::num_partial::Partial<'g>>,
    /// D3153: the generic owner's count floor (`OwnerDesc::floor`, twopass_having `owner_floor`): its one reader's HAVING's.
    floor: Option<i64>,
    mcx: Mcx<'g>,
}

/// D1533: the window a run carry reads; the input end's flush (a RIGHT/FULL merge join's unmatched inner rows) runs outside
/// every window (frame.rs `flush_path`), so its rows take the straight road (runs word 0: each row folded into the table,
/// the carry's run, closed at the last `window_end`, left as it is).
pub(crate) fn flush_win(env: &Env) -> Win {
    env.win.unwrap_or(Win { cursor: env.zero, end: env.zero, facts: crate::face::source::WinConds { runs: Some(env.zero), ..Default::default() } })
}

pub(crate) fn make<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let gx = px.gx;
    // D4620: a cascaded grouping set's end (its parent's records into the base's record, ops/agg/cascade.rs)
    if crate::ops::agg::cascade::at(gx, k).is_some() {
        return crate::ops::agg::cascade::make_end(px, k);
    }
    let node = gx.pipelines()[k].node_id;
    let Some(g) = gx.group_at(k) else { bug(node, "a hash aggregate end without its group") };
    let rec = GroupRec::new(px, g);
    if g.keys.iter().any(|key| gravity_decompose::effects::agg::hash_raises(gx.facts, key.tf, key.coll)) {
        // D1292: the plain hash end, raising at the first row hashed (hash_raise.rs)
        let end = end_of(px, k, rec, None, None, None);
        return crate::ops::agg::hash_raise::wrap(px, k, end);
    }
    // D4620: a cascade's base keeps every group it folds (its cascaded sets read them all): no front with a HAVING floor,
    // no parted owner, no HAVING split, no admitted domain
    let base = crate::ops::agg::cascade::base(gx, k);
    // V4R-375: a record holding hosted states takes no code front, no HAVING split (its partials meet at the owner alone)
    let pc = (rec.handle.is_none() && !base).then(|| crate::dict::strategies::codefront_percode::plan(px, k, &rec)).flatten(); // the front's group in one pass, its HAVING floor at the emit
    if let Some(end) = pc.and_then(|_| crate::ops::sort::topn::strategies::parted::parted(px, k, g, &rec)) { return end; } // V4R-105: at the owner
    if pc.is_none() && rec.handle.is_none() && !base {
        // D-06c: a count top's own split first (its admitted set is never larger than a HAVING floor's)
        if let Some(end) = crate::frame::iface::twopass_count_split(px, k, g, &rec).or_else(|| crate::frame::iface::twopass_having_split(px, k, g, &rec)) {
            return end;
        }
    }
    let admit = (!base).then(|| crate::frame::iface::twopass_pass2(px, k, g, &rec)).flatten();
    // D3148 (join.domain): a one-pass group only a hash join above it reads folds the rows its build's keys admit
    let admit = admit.or_else(|| (pc.is_none() && !base).then(|| crate::frame::iface::join_domain(px, k, g, &rec)).flatten());
    if pc.is_some() && admit.is_some() {
        bug(node, "a per-code front beside a planner pass 2 (the front declines a two_pass group)");
    }
    end_of(px, k, rec, admit, None, pc)
}

/// Pipeline `k`'s hash end over record `rec` (its arguments compiled here), probing `admit`, in pass `split`, through the
/// per-code front `pc` (`make`'s plan: a one-pass group's alone).
pub(crate) fn end_of<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, mut rec: GroupRec<'g, 'p>, admit: Option<crate::ops::shared::twopass::Admit>, split: Option<crate::ops::shared::twopass::Split>, pc: Option<crate::dict::strategies::codefront_percode::Percode>) -> PgBox<'g, dyn EndMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    let mcx = px.mcx();
    rec.compile_args(px, false);
    // V4R-375: hosted states fold in the hashed table's records alone (no dense / code table, run carry, constant-key
    // window or row skip), their arena declared here
    let hosted = rec.arena(px, k).is_some();
    let t = table_decl(px, k, &rec, 0);
    let (count, pf) = (!rec.sentinel_owned(), crate::table::emit_group::prefetch_keys(px, &rec));
    let one = admit.is_none() && split.is_none() && pc.is_none() && !hosted;
    let light = one.then(|| count_front_of(mcx, &node, &rec, &t, &pf).or_else(|| crate::ops::agg::emit_group_nocode::light(px, k, &rec, &t))).flatten(); // C5 S5: a path key
    let light = light.map(|(m, c, cols)| (m, crate::dict::strategies::codefront_light::with_declined(px, k, c), cols));
    let table = if pc.is_some() || light.is_some() || hosted { &crate::table::bypass::HASHED } else { crate::table::face::table(&node, &t) };
    // L0-l: a key copy is kept at its record's open, never carried; N13: nor a keyed numeric extreme's side copy; a
    // record-only class (`FoldClass::record_only`) steps into its record on the straight road
    let keyed = rec.aggs.iter().any(|a| crate::ops::shared::agg::order_keyed(&a.d));
    let carried = !keyed && !rec.reps.iter().flatten().any(|r| r.datum || r.nocode) && !rec.aggs.iter().any(|a| a.class.record_only(&a.d));
    let src = crate::table::emit_group::derived_col(px.gx, &rec, &t);
    let run = crate::ops::group::distinct_plain::run_carry(&node, &t).filter(|_| !hosted && carried);
    let run = run.filter(|_| crate::ops::shared::agg::carry_rides(px, k, crate::ops::shared::agg::KEYED_RATE));
    // D3007: a value-set group under a top by its key cuts each part's entries before they hash (code_cut.rs group form);
    // the cut lets a row through or drops it, so no run carry rides there
    // D4620: a cascade's base keeps every group (no cut, row skip, count-ordered owner or owner floor)
    let base = crate::ops::agg::cascade::base(px.gx, k);
    let cut = one && !base && light.is_none() && src.is_none() && crate::dict::strategies::code_cut::group_cut(px, k);
    let run = run.filter(|_| !cut);
    let pk = one.then(|| crate::dict::strategies::percode_const::plan(px, k, &rec, count)).flatten();
    let walk = (one && !base).then(|| crate::ops::agg::walk_skip::plan(px, k, &rec, t.state_off + table.state_bytes(&t))).flatten();
    // D-06b: a split group's count top (owner.count) cuts pass 2's records, the group's own; a pass 1 (`Split::One`, or
    // the planner's `Split::Top`) folds its counts alone
    let pass1 = matches!(split, Some(crate::ops::shared::twopass::Split::One(_) | crate::ops::shared::twopass::Split::Top(_)));
    let top = (!base && !pass1).then(|| crate::ops::sort::topn::lower::count_top_of(px, k)).flatten().map(|(m, n)| m.owner(px, k, rec.g, &t, n)); // claims words (V4R-113)
    let bare = one && pf.is_empty() && table.hashed() && run.is_none() && src.is_none() && pk.is_none() && walk.is_none() && top.is_none() && light.is_none(); // one: no pc, admit, split, hosted
    let part = crate::ops::agg::numeric::strategies::num_partial::plan(px, &rec, bare, count, t.state_off + table.state_bytes(&t));
    let plain = pk.is_none() && walk.is_none() && top.is_none() && part.is_none();
    let floor = (admit.is_none() && split.is_none() && pc.is_none() && !hosted && top.is_none() && !base).then(|| crate::ops::sort::topn::strategies::twopass_having::owner_floor(px, k, &rec).or_else(|| crate::ops::sort::topn::lower::top_floor(px, k, &rec))).flatten(); // claims K's word (the filter's)
    let end: PgBox<'g, dyn EndMech + 'g> = mcx::pg_box_dyn!(mcx, HashEnd { rec, t, table, run, count, pf, admit, hoist: admit.is_some() && crate::ops::shared::twopass::hoisted(), live: core::cell::Cell::new(None), split, src, pc, pk, top, walk, light, part, floor, mcx } => dyn EndMech + 'g);
    if cut && plain { crate::dict::strategies::code_cut::group_end(px, k, end) } else { end }
}

/// code.front_light's front for `rec` (face/keyed.rs `count_front`, V4R-184): a one-pass group (not a marked pass, no num
/// tree) whose every key is a plain code key (not derived; one key: the table's code key, two or more: its dense code
/// keys) whose Fp128 table the source declared (`pf`), where the road's `plan` takes it.
fn count_front_of<'m>(mcx: Mcx<'m>, node: &i32, rec: &GroupRec<'_, '_>, t: &TableDecl, pf: &[(i16, u8)]) -> Option<(&'static dyn CountFrontMech, CountShape<'m>, PgVec<'m, i16>)> {
    let g = rec.g;
    let plain = match rec.reps.len() {
        1 => t.key.code.is_some(),
        _ => t.key.dense.is_some_and(|d| !d.derived),
    };
    if !plain || rec.nums.as_ref().is_some_and(|n| !n.is_empty()) || g.two_pass.is_some() || g.set.is_some() || g.aggs.len() != rec.aggs.len() || !matches!(g.form, GroupForm::Hashed | GroupForm::Sorted) {
        return None;
    }
    let mut keys: PgVec<'_, CountKey> = mcx::reserved_in(rec.reps.len(), mcx);
    for r in rec.reps.iter() {
        let src = r.as_ref().filter(|r| !r.derived && !r.guard)?;
        let Rep::Code { dict } = src.rep else { return None };
        let fp_table = pf.iter().find(|p| p.0 == src.attno)?.1;
        keys.push_in(CountKey { col: src.col, lane: dict.lane, fp_table });
    }
    let m = crate::table::face::count_front(node)?;
    m.plan(t, g, &keys[..], mcx).map(|c| (m, c, keys.iter().map(|k| k.col).collect_in(mcx)))
}

/// The table pipeline `k`'s end declares: its record, owned by the records' life, its key shape (keyed_emit_group
/// `key_shape`), its Facts protocol declared (table.sized).
pub(crate) fn table_decl(px: &mut Px<'_, '_>, k: usize, rec: &GroupRec<'_, '_>, state_off: u32) -> TableDecl {
    let (nkeys, (key, lane)) = (rec.layout.nkeys(), crate::table::emit_group::key_shape(px.gx, rec, px.site_mut(k).is_some_and(|s| s.entries.is_some())));
    let copies = crate::table::emit_key::copy_slots(rec); // D3312
    let mut t = TableDecl { image_words: nkeys as u16, record_bytes: rec.layout.size() - 8 * nkeys as u32, expect: None, depth: Depth::TOP, life: rec.home, layout: rec.layout, state_off, key, facts: 0, copies };
    t.facts = crate::table::size::declare_facts(px, k, &t, lane.unwrap_or(0));
    // D2988: a table reopened per rescan (a nest's or SubPlan's rerun set) opens at the planner's numGroups when no fact
    // sizes it, not the full default table each time (zeroed at every rescan); it still grows in place past it
    if crate::rerun::sets::in_owner_set(px.gx, k) {
        t.expect = Some(rec.g.groups).filter(|&g| g > 0);
    }
    t
}

impl<'p> HashEnd<'_, 'p> {
    /// A fresh record's slots initialised and its representatives written, once, each marked valid unless a NULL source row's
    /// MINCODE_NONE (abi's `Rec::fold` merges only a valid slot: a code front's record flushed into the table).
    /// V4R-215: with `put` (the registers, every aggregate's class `puts`), the fresh arm writes each slot from them instead
    /// (no init store, no merge reload of the line just written) and ends at the returned join; the builder is left on the
    /// merge arm, which the caller ends there.
    fn open_rec(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, rec: Value, fresh: Value, reps: &[Option<Value>], put: Option<&[Value]>) -> Option<Block> {
        let (l_init, l_go) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(fresh, (l_init, &[]), (l_go, &[]));
        b.switch_to(l_init);
        let mut at = 0;
        for a in &self.rec.aggs {
            let (slot, w) = (self.rec.slot(rec, a.d.slot), a.shape.words as usize);
            match put {
                Some(r) => a.class.put(b, env, &a.d, slot, &r[at..at + w]),
                None => a.class.init(b, env, &self.rec.decl(a), slot),
            }
            at += w;
        }
        if let (Some(h), Some(word)) = (self.rec.handle, self.rec.arena) {
            // V4R-375: the arena's handle in every record, the owner fold's combine reads it (kernels m6_agg `handle_slot`)
            let v = b.load(Type::I64, env.params, 8 * word as i32);
            b.store(rec, self.rec.layout.slot_off(h as usize) as i32, v);
            let set = b.bconst(true);
            crate::ops::shared::slots::mark(b, env, rec, crate::ops::shared::slots::slot_valid(&self.rec.layout, u16::from(h)), set);
            crate::ops::agg::trans::hosted::home(b, env, &self.rec.layout, rec, v, self.rec.node); // the group's one state (D798's bound)
        }
        for (k, w) in self.rec.g.keys.iter().zip(reps).filter_map(|(k, w)| w.map(|w| (k.rep, w))) {
            b.store(rec, self.rec.layout.slot_off(k as usize) as i32, w);
            let set = { let none = b.const_raw(Type::I64, gravity_abi::breaker::MINCODE_NONE); b.icmp(IntCond::Ne, w, none) };
            crate::ops::shared::slots::mark(b, env, rec, crate::ops::shared::slots::slot_valid(&self.rec.layout, k), set);
        }
        let l_done = put.map(|_| b.create_block(&[]));
        b.br(l_done.unwrap_or(l_go), &[]);
        b.switch_to(l_go);
        l_done
    }

    /// The count's bump, skipped for a record the append opened with its count (`counted`, V4R-201). D3572: a named
    /// sentinel slot (no aggregate owns it: a two-pass top's pass-1 count(*)) under a validity word is marked valid too,
    /// as abi's `add_count` and the implicit sentinel's bump (agg trans/slots.rs `sentinel`) mark theirs: `Rec::fold`,
    /// the one merge of partial records, folds only a valid slot, so an unmarked count(*) kept its first partial's rows
    /// (an extreme top's pass 1 then never saw count(v) < count(*), and left out a key holding an all-NULL group).
    fn bump(&self, b: &mut FunctionBuilder<'_>, env: &Env, rec: Value, n: Value, counted: Value) {
        let l = &self.rec.layout;
        crate::table::radix::bump_unless(b, rec, l.sentinel_off() as i32, n, counted);
        let named = l.has_validity() && l.flags & gravity_abi::breaker::LAYOUT_IMPLICIT_COUNT == 0;
        if let Some(j) = (0..l.nslots()).find(|&j| named && l.slot_off(j) == l.sentinel_off()) {
            let any = b.icmp(IntCond::Ne, n, env.zero);
            crate::ops::shared::slots::mark(b, env, rec, crate::ops::shared::slots::slot_valid(l, j as u16), any);
        }
    }

    /// The row's fold (`lower`, past a walked group's row skip).
    fn lower_row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals) -> Carry<'e> {
        if let Some((m, shape, cols)) = self.light.as_ref().filter(|_| env.mult.is_none()) {
            // code.front_light: the front counts the row's codes; the image, its hash and the fold are the table arm's alone
            let cells: PgVec<'_, _> = cols.iter().map(|&col| row.col(col).unwrap_or_else(|| bug(self.rec.node, "a count front's key the row lacks"))).collect_in(self.mcx);
            let l_done = m.row(b, env, &|b, env| self.light_words(b, env).1, shape, &cells[..], row.at);
            let img = self.rec.image(b, env, row);
            let h = emit_key_hash(b, &img.words);
            self.row_into(b, env, row, &img, h);
            b.br(l_done, &[]);
            b.switch_to(l_done);
            let one = b.iconst(Type::I64, 1);
            return Carry::new(b.int_bin(IntBinOp::Add, row.carry.out, one), env.mcx);
        }
        crate::frame::prefetch_fp::ahead(b, env, row.at, &self.pf);
        let one = b.iconst(Type::I64, 1);
        let out = b.int_bin(IntBinOp::Add, row.carry.out, one);
        if self.run(env).is_none() && self.row_lazy(b, env, row) {
            return Carry::new(out, env.mcx); // D3224 A4: a code front's probe
        }
        let mut img = self.rec.image(b, env, row);
        crate::table::emit_group::derived_index(b, env, self.rec.node, row, self.src, &mut img);
        match self.run(env) {
            Some(run) => {
                let win = flush_win(env);
                let accs = run.row(b, env, &self.t, &win, row, &img, &row.carry.accs, self);
                Carry { out, accs }
            }
            None => {
                let pc = self.pc.filter(|_| env.mult.is_none());
                let f = |b: &mut FunctionBuilder<'_>, env: &mut Env| pc?.f_value(b, env, &self.rec, row);
                let l_done = pc.map(|p| p.row(b, env, &self.t, &img, f));
                let l_pair = self.part.as_ref().and_then(|p| p.pair(b, env, &self.rec, row, &img)); // num.partial P2: before the hash
                let h = emit_key_hash(b, &img.words);
                self.row_into(b, env, row, &img, h);
                if let Some(l) = l_pair {
                    b.br(l, &[]);
                    b.switch_to(l);
                }
                if let Some(l) = l_done {
                    b.br(l, &[]);
                    b.switch_to(l);
                }
                Carry::new(out, env.mcx)
            }
        }
    }

    /// The open run's carry, when `run.collapse` is on and the frame carries registers across rows (`frame.carry`).
    fn run(&self, env: &Env) -> Option<&'static dyn RunCarry> {
        self.run.filter(|_| self.light.is_none() && env.applies(FRAME_CARRY))
    }

    /// The table's state and the count front's words after it.
    fn light_words(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> (Value, Value) {
        let st = crate::table::bypass::state(b, env, &self.t);
        (st, b.ptr_add(st, env.zero, 1, (self.table.state_bytes(&self.t) + self.walk.map_or(0, |_| 8)) as i32))
    }

    /// The registers' layout: each aggregate's words, one word per representative, the row count when the end counts.
    fn nreps(&self) -> usize {
        self.rec.reps.iter().flatten().count()
    }

    fn close_in(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, img: &ImageWords, h: Value, accs: &[Value]) {
        let tested = self.admits_runs(env); // the run carry admitted this run at its open
        let l_join = crate::frame::iface::twopass_admitted_at(self.admit.filter(|_| !env.every && !tested), b, env, &self.rec, img, self.live.get());
        let base: usize = self.rec.aggs.iter().map(|a| a.shape.words as usize).sum();
        let (rec, fresh, counted) = self.table.upsert_n(b, env, &self.t, img, h, self.count.then(|| accs[base + self.nreps()]));
        let mut reps = accs[base..].iter().copied();
        let words: PgVec<'_, Option<Value>> = self.rec.reps.iter().map(|r| r.and_then(|_| reps.next())).collect_in(self.mcx);
        // V4R-215: a record with aggregates that all put takes a fresh record's slots from the registers (an agg-less one keeps
        // the one arm: a `[key, count]` pair)
        let put = (!self.rec.aggs.is_empty() && self.rec.aggs.iter().all(|a| a.class.puts(&a.d))).then_some(accs);
        let l_done = self.open_rec(b, env, rec, fresh, &words[..], put);
        let (mut at, images) = (0, self.rec.images(b, env, &self.t));
        for a in &self.rec.aggs {
            let w = a.shape.words as usize;
            let mut r = mcx::to_vec_in(&accs[at..at + w], self.mcx);
            crate::dict::minmax_code::merge_with(a.class, b, env, &a.d, self.rec.slot(rec, a.d.slot), Acc::Regs(&mut r[..]), images);
            at += w;
        }
        if let Some(l) = l_done {
            b.br(l, &[]);
            b.switch_to(l);
        }
        if self.count {
            self.bump(b, env, rec, accs[base + self.nreps()], counted);
        }
        crate::ops::sort::topn::strategies::twopass::join(b, l_join);
    }

    fn row_in(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, img: &ImageWords, h: Value) {
        if self.rec.arena.is_some() {
            env.agg_arena = self.rec.arena; // V4R-375: the hosted classes' init and step call through it
        }
        let l_join = crate::frame::iface::twopass_admitted_at(self.admit.filter(|_| !env.every), b, env, &self.rec, img, self.live.get());
        let front = self.part.as_ref().and_then(|p| p.row(b, env, &self.rec, row, img, h, (self.table, &self.t), self)); // num.partial
        let m = env.mult.unwrap_or(env.one);
        env.rec_window = None;
        let (rec, fresh, counted) = self.table.upsert_n(b, env, &self.t, img, h, self.count.then_some(m));
        let window = env.rec_window.take();
        let reps = crate::table::emit_group::rep_words(&self.rec, b, env, row);
        let reps = crate::table::emit_key::keep_datums(&self.rec, b, env, row, fresh, &self.t, reps);
        self.open_rec(b, env, rec, fresh, &reps, None);
        self.fold_rec(b, env, row, rec, counted, window);
        self.part.as_ref().zip(front.as_ref()).inspect(|(p, f)| p.claim(b, env, rec, img, h, f));
        crate::ops::sort::topn::strategies::twopass::join(b, l_join);
    }

    /// The row's steps into the open record `rec` (`Env::rec_zeroed`; `window`: `Env::rec_window`, a code front's arm) and
    /// the count's bump unless the append counted it (`counted`).
    fn fold_rec(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, rec: Value, counted: Value, window: Option<Value>) {
        let images = self.rec.images(b, env, &self.t);
        let zeroed = core::mem::replace(&mut env.rec_zeroed, true);
        env.rec_window = window;
        for a in &self.rec.aggs {
            self.rec.step(b, env, row, a, AccAt::Slot(rec, a.d.slot, images));
        }
        (env.rec_zeroed, env.rec_window) = (zeroed, None);
        if self.count {
            let m = env.mult.unwrap_or(env.one);
            self.bump(b, env, rec, m, counted);
        }
    }

    /// D3224 A4: a code front's row with no key image while its record is open (`TableMech::probe` over the codes alone,
    /// GroupRec `front_codes`): the image, its hash, the representatives and the record's open run in a cold block, for
    /// a fresh record or a code past the front, which joins the steps (`fold_rec`) with the record. Only where no admitted
    /// set, num.partial front, per-code cut or derived key reads the image. `false`: no probe, nothing emitted that stays.
    fn row_lazy(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> bool {
        if self.admit.is_some() || self.part.is_some() || self.pc.is_some() || self.src.is_some() {
            return false;
        }
        let Some(codes) = self.rec.front_codes(b, env, row) else { return false };
        let Some((l_hot, l_cold)) = self.table.probe(b, env, &self.t, &codes) else { return false };
        env.aside(|env| {
            if self.rec.arena.is_some() {
                env.agg_arena = self.rec.arena; // V4R-375: the hosted classes' init and step call through it
            }
            let l_join = b.create_block(&[Type::Ptr, Type::Bool, Type::Bool]);
            b.switch_to(l_cold);
            env.aside(|env| {
                let img = self.rec.image(b, env, row);
                let h = emit_key_hash(b, &img.words);
                let m = env.mult.unwrap_or(env.one);
                env.rec_window = None;
                env.front_probed = true; // the probe missed the run cache: the upsert refills it, never reads it
                let (rec, fresh, counted) = self.table.upsert_n(b, env, &self.t, &img, h, self.count.then_some(m));
                env.front_probed = false;
                let window = env.rec_window.take().unwrap_or_else(|| b.bconst(false));
                let reps = crate::table::emit_group::rep_words(&self.rec, b, env, row);
                let reps = crate::table::emit_key::keep_datums(&self.rec, b, env, row, fresh, &self.t, reps);
                self.open_rec(b, env, rec, fresh, &reps, None);
                b.br(l_join, &[rec, counted, window]);
            });
            b.switch_to(l_hot);
            let (rec, window, no) = (b.block_params(l_hot)[0], b.block_params(l_hot)[1], b.bconst(false));
            b.br(l_join, &[rec, no, window]);
            b.switch_to(l_join);
            let p = mcx::to_vec_in(&b.block_params(l_join)[..], self.mcx);
            self.fold_rec(b, env, row, p[0], p[1], Some(p[2]));
        });
        true
    }
}

impl<'p> RunFold for HashEnd<'_, 'p> {
    fn init<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> PgVec<'e, Value> {
        let mut accs = PgVec::new_in(env.mcx);
        for a in &self.rec.aggs {
            let mut r = mcx::pg_vec![in self.mcx; env.zero; a.shape.words as usize];
            a.class.init(b, env, &a.d, Acc::Regs(&mut r[..]));
            accs.extend_in(r);
        }
        accs.extend_in(core::iter::repeat_n(env.zero, self.nreps() + self.count as usize));
        accs
    }

    /// The run carry calls `step`, `close` and `row_into` each in a branch of the row: each sets the row's memo aside.
    fn step<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, accs: &[Value]) -> PgVec<'e, Value> {
        env.aside(|env| {
            let mut out = mcx::to_vec_in(accs, env.mcx);
            let mut at = 0;
            for a in &self.rec.aggs {
                let w = a.shape.words as usize;
                self.rec.step(b, env, row, a, AccAt::Regs(&mut out[at..at + w]));
                at += w;
            }
            for w in crate::table::emit_group::rep_words(&self.rec, b, env, row).into_iter().flatten() {
                out[at] = w;
                at += 1;
            }
            if self.count {
                let m = env.mult.unwrap_or(env.one);
                out[at] = b.int_bin(IntBinOp::Add, out[at], m);
            }
            out
        })
    }

    fn close(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, img: &ImageWords, h: Value, accs: &[Value]) {
        env.aside(|env| self.close_in(b, env, img, h, accs));
    }

    fn row_into(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, img: &ImageWords, h: Value) {
        env.aside(|env| self.row_in(b, env, row, img, h));
    }

    fn admits_runs(&self, env: &Env) -> bool {
        self.hoist && self.admit.is_some() && !env.every
    }

    fn admit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, img: &ImageWords) -> Value {
        let Some(a) = self.admit else { bug(self.rec.node, "a run admission without the end's admitted set") };
        env.aside(|env| a.probe(b, env, &self.rec, img, self.live.get()))
    }
}

impl<'p> EndMech for HashEnd<'_, 'p> {
    /// join.mult (H20): every aggregate folds `m` copies exactly (fold_plain `weighted`), the count sentinel adds `m`; a
    /// DISTINCT pair record is not a copy count.
    fn takes_mult(&self) -> bool {
        self.rec.g.form != GroupForm::Distinct && self.rec.aggs.iter().all(|a| crate::ops::agg::trans::plain::weighted(&a.d) && a.filter.is_none())
    }

    /// join.dedup: a group of keys alone (no aggregate, no two-pass count, no grouping set) keeps one row per key whatever
    /// its count.
    fn takes_set(&self) -> bool {
        let g = &self.rec.g;
        matches!(g.form, GroupForm::Hashed | GroupForm::Sorted | GroupForm::Group | GroupForm::Unique) && g.aggs.is_empty() && self.rec.aggs.is_empty() && g.two_pass.is_none() && g.set.is_none() && self.admit.is_none() && self.split.is_none()
    }

    /// `lower_row` adds one per row on each of its paths and a whole window its rows (`whole`, as the frame's count
    /// reads it); the group walk's skip (walk_skip.rs) leaves the carry as it was for a row that entered the end: not then.
    fn out_counts_rows(&self) -> bool {
        self.walk.is_none()
    }

    fn state_bytes(&self) -> u32 {
        self.table.state_bytes(&self.t) + self.pc.map_or(0, |_| crate::dict::strategies::codefront_percode::WORDS) + self.walk.map_or(0, |_| 8) + self.light.as_ref().map_or(0, |l| l.0.state_bytes(&l.1)) + self.part.as_ref().map_or(0, |p| p.state_bytes())
    }

    fn open_words(&self, w: &mut OpenWords) {
        self.table.open_words(&self.t, w);
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.admit.inspect(|a| a.check_open(b, env));
        if let Some(word) = self.rec.handle.and(self.rec.arena) {
            crate::ops::agg::trans::hosted::home_open(b, env, word); // V4R-375: a fresh table's run forgets the last run's homes
        }
        self.table.open(b, env, &self.t);
        self.pc.inspect(|p| p.open(b, env, &self.t));
        self.walk.inspect(|w| w.open(b, env, &self.t));
        self.part.as_ref().inspect(|p| p.open(b, env));
        if let Some((m, shape, _)) = self.light.as_ref() {
            let (_, w) = self.light_words(b, env);
            m.open(b, env, w, shape);
        }
    }

    fn carried<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> PgVec<'e, Value> {
        match self.run(env) {
            Some(run) => run.carried(b, env, &self.t, self),
            None => PgVec::new_in(env.mcx),
        }
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        match self.walk {
            Some(w) => w.row(b, env, &self.rec, row, |b, env| self.lower_row(b, env, row)),
            None => self.lower_row(b, env, row),
        }
    }

    /// D3326: also the row loop's start, where an admitting end reads its set's header words once for the rows below it.
    fn whole(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win, c: &Carry, l_wend: Block) -> bool {
        self.live.set(self.admit.filter(|_| self.hoist && !env.every).map(|a| a.live(b, env)));
        if self.live.get().is_some() {
            fires::fire("admit.live"); // shapecheck fire signal (D3326): the admitted set's header read once a row loop
        }
        if let Some((m, shape, _)) = self.light.as_ref() {
            let (st, w) = self.light_words(b, env);
            m.bind(b, env, st, w, shape);
        }
        self.pk.as_ref().is_some_and(|p| p.whole(b, env, &self.rec, self, win, c, l_wend)) || self.pc.is_some_and(|p| crate::dict::strategies::countwalk::whole(&p, b, env, &self.rec, &self.t, win, c, l_wend))
    }

    fn every_guard(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Option<Value> {
        self.admit.filter(|a| a.every_window).map(|a| a.every(b, env))
    }

    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, c: &Carry) {
        if let Some(run) = self.run(env) {
            run.window_end(b, env, &self.t, &c.accs, self);
        }
        self.table.window_end(b, env, &self.t);
        self.pc.inspect(|p| p.window_end(b, env, &self.t));
        self.live.set(None); // the row loop's header words dominate its own rows alone
    }

    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        if let Some((m, shape, _)) = self.light.as_ref() {
            let (st, w) = self.light_words(b, env);
            m.finish(b, env, st, w, shape, x);
        }
        self.pc.inspect(|p| p.finish(b, env, &self.t, x));
        self.part.as_ref().inspect(|p| p.flush(b, env, self)); // num.partial's blocks into the table
        self.table.finish(b, env, &self.t, x);
    }

    /// The owner phase merges the record as the table holds it (image keys, image min / max slots). The graph's legacy
    /// kernel fact words (decompose agg.rs `W_*`) go: they name slots of v4's own word plan (v4's facts are protocols).
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize, mut d: Pipeline) -> Pipeline {
        d.words = None;
        d = crate::table::emit_key::resident(&self.rec, d);
        if let Some(o) = d.owner.as_mut() {
            (o.layout, o.acc) = (self.t.layout, self.t.layout);
            *o = match self.top { Some(t) => t, None => crate::ops::sort::topn::strategies::parted::owner(px, &self.rec, *o) };
            // SRC-38 slice 2: the generic owners of a dense-capable table fold the key range the Observe proto remembers (a
            // constructor's never; a hashed-only table, a front's or a hosted one, has no use for it)
            let generic = !self.table.hashed() && o.pair == 0 && o.ctor.is_none() && o.ctor_desc.is_none();
            o.observe_key = if generic { crate::table::emit_group::observes(px.gx, &self.rec, self.t.key.domain) } else { gravity_program::OBSERVE_NONE };
            // D3153: the generic runs form lays only the records over its reader's count floor (D-06: or its top's)
            if let Some(f) = self.floor {
                let floored = gravity_program::OwnerDesc { floor: f, ..*o };
                if floored.runs_floor_of().is_some() {
                    fires::fire("owner.having");
                    *o = floored;
                } else if floored.runs_top_of().is_some() {
                    fires::fire("owner.top");
                    *o = floored;
                }
            }
        }
        self.split.map_or(d, |s| s.desc(px.arena, self.rec.node, d))
    }
}
