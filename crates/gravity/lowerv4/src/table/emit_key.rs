//! The group emit's key cells (split from keyed/emit_group.rs by M6 L0-l, V4R-272): how each group key's output cell is
//! made from its record. A by-value key is its image word; D531's integral extract is that word as a scale-0 numeric; a
//! text key with a dictionary is its representative code, rendered through the producer's dictionary; a derived text key
//! is the key expression re-evaluated over that render; a varlena key with no code (L0-l) is the copy of its own value
//! the record keeps once per group (`keep_datums`), emitted as that datum and sorted as it (a group sort's `SK_HEAP`
//! text word). The copy is the value's payload behind a 4-byte header (PG's pg_detoast_datum_packed then VARDATA_ANY:
//! a compressed or TOAST value detoasted once, at the record's open), in the table's arena (the images cursor), never a
//! copy per row. Equality stays the key image's (M6-plan Decision 3). A table holding copies never spills (a spilled
//! partition does not carry them, L0-j): it grows its grant instead (D6). Registers nothing. Owner M1-KEYED.
use gravity_abi::breaker::{KeyKind, MINCODE_NONE};
use gravity_decompose::ops::shared::agg::GroupDecl;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::{ExprLife, ExprOut, NumTypmod, NumForm, Pipeline, Rep, Until};
use mcx::{CollectIn, GrowIn, PgVec};
use types_core::catalog::NUMERICOID;

use crate::cell::{At, Cell, CellHome, CellLife, Nullable, Row};
use crate::table::face::TableDecl;
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::frame::row::RowVals;
use crate::admit::refusal::{Reason, ShapeWhy};

use crate::ops::agg::hashagg::GroupRec;

/// How a key's output cell is made from its record.
pub(crate) enum KeyOut<'g> {
    /// A by-value key: its image word is its datum.
    Word { word: usize },
    /// D531: an integral extract's integer word (`extract_word_key`), rendered as its numeric (a scale-0 packed word);
    /// M6-CLOSE (a'): a packed lane's key, its mantissa word rendered at the lane's facts (`num`).
    Num { word: usize, num: NumTypmod },
    /// D3149: D531's word of a monotonic unit (year, decade, century, millennium): gravity_expr's WORD_NINF / WORD_PINF
    /// (an infinite source's ±Infinity) leave as the packed sentinels MANT_NINF / MANT_PINF, any other word as itself; a
    /// scale-0 packed word computed at the emit (the record keeps the unmapped word).
    NumInf { word: usize },
    /// A text key: its representative code, rendered through the producer's dictionary; D3324 (b) (`home`): a dependent
    /// key's build-row code (ops/agg/strategies/dependent.rs), through that build's.
    Code { slot: u16, rep: Rep, home: Option<gravity_program::LifeId> },
    /// A derived text key: the key expression re-evaluated over the render of its code column `attno`. D3855: the render
    /// row holds that render at `ex_attno` too, the column the expression's own Var reads (emit_group.rs `derived_attno`).
    Derived { slot: u16, rep: Rep, attno: i16, ex_attno: i16, typid: u32, ex: ExId, els: PgVec<'g, ExId>, out: Cell },
    /// L0-l: a varlena key with no code: the record's copy of its value (a `VarlenaPtr` word; the key's flag its NULL);
    /// L0-m: a numeric key's copy, or (`word`) a float key's row word, one member's (Decision 7). textkeys step 3
    /// (`bare`, road key.canon): a slot of 0 over a non-NULL value is a value of at most 15 bytes the record keeps no copy
    /// of, rendered from the key's two canonical image words (rep/varlena/key_canon.rs `render`).
    Datum { slot: u16, word: bool, bare: bool },
    /// A jsonb path key: its code rendered, or its kept text (keyed/emit_group_nocode.rs).
    Nocode(crate::ops::agg::emit_group_nocode::Out),
    /// D3080 (H025): an integer's text keyed by the integer (decompose `KeyDecl::int_text`): the cast re-evaluated over the
    /// record's integer word (column `attno` of a one-cell row), once per emitted group.
    IntText { word: usize, attno: i16, typid: u32, ex: ExId, out: Cell },
    /// D2474: a macaddr / macaddr8 key, its record word rep.hosted.order.fixed's exact order word (rep/hosted/order.rs
    /// `word_key_len`): its `len` bytes rendered back from the word into a frame slot, the datum that slot's address.
    Bytes { word: usize, len: u16 },
}

/// Group `g`'s key outputs over its emit record `rec`, for the emit of pipeline node `node`.
pub(crate) fn key_outs<'g, 'p>(px: &mut Px<'g, 'p>, node: i32, g: &'g GroupDecl<'p>, rec: &GroupRec<'g, 'p>) -> PgVec<'g, KeyOut<'g>> {
    let gx = px.gx;
    let mut keys = mcx::reserved_in(g.keys.len(), px.mcx());
    for (i, kd) in g.keys.iter().enumerate() {
        keys.push_in(match rec.reps[i] {
            Some(r) if r.datum => KeyOut::Datum { slot: crate::frame::pairs_view::rep_slot(g, kd.rep), word: kd.image.is_float(), bare: rec.bare >> i & 1 == 1 },
            Some(r) if r.nocode => KeyOut::Nocode(crate::ops::agg::emit_group_nocode::out(gx, g, kd, r.rep, node)),
            Some(r) if r.derived => {
                let Some(o) = gx.out_of(kd.expr.node()) else { crate::refuse!(node, Reason::Shape { why: ShapeWhy("a derived group key without an output rep") }) };
                // a top's drain compiles the key again: its literals keep their words (step/consts.rs, D317)
                let (ex, els) = crate::dict::strategies::percode_guard::render_exs(px, kd.expr.node(), node, r.guard);
                let ex_attno = crate::table::emit_group::derived_attno(gx, g.producer.0 as usize, node, kd.expr.node()).unwrap_or(r.attno);
                KeyOut::Derived { slot: crate::frame::pairs_view::rep_slot(g, kd.rep), rep: r.rep, attno: r.attno, ex_attno, typid: r.typid, ex, els, out: Cell::of_expr(o, 0, 0, Nullable::Maybe) }
            }
            Some(r) => KeyOut::Code { slot: crate::frame::pairs_view::rep_slot(g, kd.rep), rep: r.rep, home: dep_home(rec, i) },
            None if kd.int_text() => {
                let Some(v) = gravity_decompose::ops::shared::agg::int_text_arg(kd.expr.node()).and_then(|a| a.as_var()) else { crate::refuse!(node, Reason::Bug { payload: "an integer-text group key without its column" }) };
                let o = crate::step::project::out_of(gx, kd.expr.node(), node);
                let ex = crate::step::expr::compile(px, kd.expr.node(), None, node);
                KeyOut::IntText { word: rec.key_words(i).0, attno: v.varattno, typid: v.vartype, ex, out: Cell::of_expr(o, 0, 0, Nullable::Maybe) }
            }
            None if kd.kind == KeyKind::Code => crate::refuse!(node, Reason::Shape { why: ShapeWhy("a same-dictionary code group key (its code has no part)") }),
            None if crate::table::emit_group::extract_word_monotonic(gx, g.producer.0 as usize, g, kd) => {
                fires::fire("expr.word.monotonic"); // D3149 witness signal: a monotonic unit's extract key keyed by its word
                KeyOut::NumInf { word: rec.key_words(i).0 }
            }
            None if crate::table::emit_group::extract_word_numeric(gx, g.producer.0 as usize, g, kd) => KeyOut::Num { word: rec.key_words(i).0, num: NumTypmod::of(19, 0) },
            // D1326: an integral extract keyed through its numeric cell (rep.hosted.hash's word; ±Infinity its sentinels)
            None if crate::table::key_copy::extract_numeric_key(kd.kind, kd.tf, kd.expr.node()) => KeyOut::Num { word: rec.key_words(i).0, num: NumTypmod::of(19, 0) },
            None => match crate::ops::sort::topn::copy_words::packed_key(gx, g.producer.0 as usize, kd) {
                Some(num) => KeyOut::Num { word: rec.key_words(i).0, num }, // M6-CLOSE (a'): the lane's mantissa word
                None => match crate::rep::hosted::order::word_key_len(&rec.tfs[i]) {
                    Some(len) => KeyOut::Bytes { word: rec.key_words(i).0, len }, // D2474: never the word as the datum
                    None => KeyOut::Word { word: rec.key_words(i).0 },
                },
            },
        });
    }
    keys
}

/// D3324 (b) / R3: the build row whose dictionary dependent / carried key `i`'s code renders through (`rec.dep`), `None`
/// for any other key (it renders through the group's producer, `rec.home`).
pub(crate) fn dep_home(rec: &GroupRec<'_, '_>, i: usize) -> Option<gravity_program::LifeId> {
    rec.dep.as_ref().filter(|d| d.has(i)).map(|d| gravity_program::LifeId(d.u.b as u16))
}

/// D4669: the life whose site key `i`'s code word renders through (a sort word's site, ops/sort/topn/lower.rs): a
/// dependent / carried key's build row, else the group's producer.
pub(crate) fn code_home(rec: &GroupRec<'_, '_>, i: usize) -> gravity_program::LifeId {
    dep_home(rec, i).unwrap_or(rec.home)
}

/// Key `i`'s cell of the record at `at` (`null`: the key's flag, when the image carries flags), for the emit of `node`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn key_cell(rec: &GroupRec<'_, '_>, keys: &[KeyOut<'_>], node: i32, b: &mut FunctionBuilder<'_>, env: &mut Env, i: usize, at: Value, null: Option<Value>, row: &RowVals) -> (Cell, Value, Option<Value>) {
    let kd = &rec.g.keys[i];
    let life = rec.home;
    let nn = if null.is_some() { Nullable::Maybe } else { Nullable::Never };
    match keys[i] {
        KeyOut::Word { word } => {
            let v = b.load(Type::I64, at, 8 * word as i32);
            (Cell::of_record(Rep::Word { bytes: 8 }, kd.tf, NumTypmod::NONE, life, word as u16, nn), v, null)
        }
        KeyOut::Num { word, num } => {
            let v = b.load(Type::I64, at, 8 * word as i32);
            let rep = Rep::PackedNum { facts: num, form: NumForm::Word };
            (Cell::of_record(rep, NUMERICOID, num, life, word as u16, nn), v, null)
        }
        KeyOut::NumInf { word } => {
            use gravity_expr::emit::prim::datetime::{WORD_NINF, WORD_PINF};
            use gravity_fn::agg_final::{MANT_NINF, MANT_PINF};
            let w = b.load(Type::I64, at, 8 * word as i32);
            let (ninf, pinf) = (b.iconst(Type::I64, WORD_NINF), b.iconst(Type::I64, WORD_PINF));
            let (mninf, mpinf) = (b.iconst(Type::I64, MANT_NINF), b.iconst(Type::I64, MANT_PINF));
            let neg = b.icmp(IntCond::Eq, w, ninf);
            let v = b.select(neg, mninf, w);
            let pos = b.icmp(IntCond::Eq, w, pinf);
            let v = b.select(pos, mpinf, v);
            let num = NumTypmod::of(19, 0);
            let out = ExprOut { rep: Rep::PackedNum { facts: num, form: NumForm::Word }, typid: NUMERICOID, life: ExprLife::Row };
            (Cell::of_expr(out, 0, 0, nn), v, null)
        }
        KeyOut::Code { slot, rep, home } => {
            let v = b.load(Type::I64, at, rec.layout.slot_off(slot as usize) as i32);
            (Cell::of_record(rep, kd.tf, NumTypmod::NONE, home.unwrap_or(life), slot, nn), v, null)
        }
        KeyOut::Datum { slot, bare: true, .. } => {
            // textkeys step 3: no copy (a slot of 0) renders the value from its canonical image, valid for the row
            let v = b.load(Type::I64, at, rec.layout.slot_off(slot as usize) as i32);
            let w = rec.key_words(i).0 as i32;
            let (w0, w1) = (b.load(Type::I64, at, 8 * w), b.load(Type::I64, at, 8 * (w + 1)));
            let r = crate::rep::varlena::key_canon::render(b, w0, w1);
            let none = b.icmp(IntCond::Eq, v, env.zero);
            let v = b.select(none, r, v);
            let life = CellLife { until: Until::Row, home: CellHome::FrameSlot };
            (Cell { rep: Rep::VarlenaPtr, typid: kd.tf, num: NumTypmod::NONE, life, at: At::Slot(slot), null: nn }, v, null)
        }
        KeyOut::Datum { slot, word, .. } => {
            let v = b.load(Type::I64, at, rec.layout.slot_off(slot as usize) as i32);
            let rep = match crate::table::key_copy::fixed_of(rec, i) {
                Some(len) => Rep::FixedPtr { len }, // M6-TYPES R1c: a uuid / interval / timetz key's kept image; M6-HOSTED: a name's, a macaddr's
                None if word => Rep::Word { bytes: 8 },
                None => Rep::VarlenaPtr,
            };
            (Cell::of_record(rep, kd.tf, NumTypmod::NONE, life, slot, nn), v, null)
        }
        KeyOut::Nocode(ref o) => crate::ops::agg::emit_group_nocode::cell(rec, o, b, env, i, at, null),
        KeyOut::Bytes { word, len } => {
            let w = b.load(Type::I64, at, 8 * word as i32);
            let v = crate::rep::hosted::order::bytes_of_word(b, w, len);
            let life = CellLife { until: Until::Row, home: CellHome::FrameSlot };
            (Cell { rep: Rep::FixedPtr { len }, typid: kd.tf, num: NumTypmod::NONE, life, at: At::Slot(word as u16), null: nn }, v, null)
        }
        KeyOut::IntText { word, attno, typid, ex, out } => {
            let w = b.load(Type::I64, at, 8 * word as i32);
            let cell = Cell::of_record(Rep::Word { bytes: 8 }, typid, NumTypmod::NONE, life, word as u16, nn);
            let cols = (1..=attno).map(|a| (a == attno).then_some(0)).collect_in(env.mcx);
            let src = RowVals { row: Row { cells: mcx::pg_vec![in env.mcx; cell] }, vals: mcx::pg_vec![in env.mcx; (w, null)], cols, carry: row.carry.copy_in(env.mcx), next: row.next, at: row.at };
            let (v, valid) = crate::step::expr::emit(b, env, node, &src, ex);
            let t = b.bconst(true);
            let n = b.int_bin(IntBinOp::Xor, valid, t);
            (out, v, Some(null.map_or(n, |f| b.int_bin(IntBinOp::Or, f, n))))
        }
        KeyOut::Derived { slot, rep, attno, ex_attno, typid, ex, ref els, out } => {
            let (w, t) = (b.load(Type::I64, at, rec.layout.slot_off(slot as usize) as i32), b.bconst(true));
            let set = match rec.layout.has_validity() {
                true => crate::ops::shared::slots::slot_ok(b, env, at, crate::ops::shared::slots::slot_valid(&rec.layout, slot)),
                false => { let none = b.const_raw(Type::I64, MINCODE_NONE); b.icmp(IntCond::Ne, w, none) } // unset (rep_words)
            };
            let code = Cell::of_record(rep, typid, NumTypmod::NONE, life, slot, Nullable::Maybe);
            // D3855: and at `ex_attno`, where the expression's Var reads it (a DISTINCT argument over the projection's column)
            let cols = (1..=attno.max(ex_attno)).map(|a| (a == attno || a == ex_attno).then_some(0)).collect_in(env.mcx);
            let src = RowVals { row: Row { cells: mcx::pg_vec![in env.mcx; code] }, vals: mcx::pg_vec![in env.mcx; (w, Some(b.int_bin(IntBinOp::Xor, set, t)))], cols, carry: row.carry.copy_in(env.mcx), next: row.next, at: row.at };
            let (v, valid) = crate::dict::strategies::percode_guard::render(b, env, node, &src, ex, els, at, rec.key_words(i).0);
            let n = b.int_bin(IntBinOp::Xor, valid, t);
            (out, v, Some(null.map_or(n, |f| b.int_bin(IntBinOp::Or, f, n))))
        }
    }
}

/// `reps` (keyed_emit_group `rep_words`) with each datum key's word its copy (a float's: its row word, L0-m), made only
/// when the record is `fresh` (the table `t`'s arena cursor, `GroupRec::images`); a NULL value's word is 0, the key's flag
/// its NULL. A copied key whose row cell is not a varlena (a kept code off its lane and off a record, a packed numeric under
/// a mantissa decl) refuses by shape; D3848: a packed numeric under the scale-free decl is copied from rep.packed.datum's image.
#[allow(clippy::too_many_arguments)]
pub(crate) fn keep_datums<'e>(rec: &GroupRec<'_, '_>, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, fresh: Value, t: &TableDecl, reps: PgVec<'e, Option<Value>>) -> PgVec<'e, Option<Value>> {
    keep_datums_in(rec, b, env, row, fresh, &mut |b, env| rec.images(b, env, t), reps)
}

/// `keep_datums` with the images cursor `images` gives (D1071: a streamed group's own, keyed/stream.rs).
#[allow(clippy::too_many_arguments)]
pub(crate) fn keep_datums_in<'e>(rec: &GroupRec<'_, '_>, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, fresh: Value, images: &mut dyn FnMut(&mut FunctionBuilder<'_>, &Env) -> Option<Value>, mut reps: PgVec<'e, Option<Value>>) -> PgVec<'e, Option<Value>> {
    reps = crate::ops::agg::emit_group_nocode::keep(rec, b, env, row, fresh, images, reps);
    let mcx = env.mcx;
    let keyed: PgVec<'_, usize> = (0..rec.reps.len()).filter(|&i| rec.reps[i].is_some_and(|r| r.datum)).collect_in(mcx);
    if keyed.is_empty() {
        return reps;
    }
    let any = keyed.iter().any(|&i| copies(rec, i));
    let images = match any.then(|| images(b, env)) {
        Some(Some(i)) => i,
        Some(None) => crate::refuse!(rec.node, Reason::Bug { payload: "a kept key copy with no images cursor" }),
        None => env.zero, // floats only: no copy
    };
    let h = any.then(|| b.declare_host_once("key_copy", &[Type::Ptr, Type::Ptr, Type::I64], Some(Type::I64), crate::table::key_copy::key_copy as *const () as usize, HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() }));
    let (l_copy, l_join) = (b.create_block(&[]), b.create_block(&mcx::pg_vec![in mcx; Type::I64; keyed.len()][..]));
    b.cond_br(fresh, (l_copy, &[]), (l_join, &mcx::pg_vec![in mcx; env.zero; keyed.len()][..]));
    b.switch_to(l_copy);
    let memo = env.memo.set_aside(); // the copy block's reads are its own
    let mut kept: PgVec<'_, Value> = mcx::reserved_in(keyed.len(), mcx);
    for &i in &keyed {
        let c = row.col(rec.reps[i].map_or(0, |r| r.col)).unwrap_or_else(|| crate::refuse!(rec.node, Reason::Bug { payload: "a kept key column the row does not carry" }));
        let copy = copies(rec, i);
        let fixed = crate::table::key_copy::fixed_of(rec, i).filter(|&n| c.cell.rep == Rep::FixedPtr { len: n }); // M6-TYPES R1c
        // blocker 4 / D1348: a Code cell renders through its producer's dictionary (Lx::site_of): a record's by pipe_sites, a lane's (At::Lane) by this frame's one source
        // D3848: a packed lane's cell under the scale-free numeric key (keysem `scale_free`) renders through
        // rep.packed.datum (the image PostgreSQL stores for the value, at the lane's dscale), which the copy takes
        // D3857: or any packed key the record keeps in two words (its Fp128: rec.rs `image_of`, keyimg.rs `packed_set_key`)
        let packed = matches!(c.cell.rep, Rep::PackedNum { .. }) && (gravity_decompose::catalog::keysem::image::scale_free(&rec.g.keys[i]) || rec.wpk.get(i) == Some(&2));
        if copy && fixed.is_none() && !packed && !matches!(c.cell.rep, Rep::VarlenaPtr) && !(matches!(c.cell.rep, Rep::Code { .. }) && (matches!(c.cell.life.home, CellHome::Record(_)) || matches!(c.cell.at, At::Lane(_)))) {
            crate::refuse!(rec.node, Reason::Shape { why: ShapeWhy(if matches!(c.cell.rep, Rep::Code { .. }) { "a text group key over a kept dictionary code off its lane and off a record (no site of its dictionary is known here)" } else { "a numeric group key over a packed lane (no copy of it renders before rep.packed.hash)" }) });
        }
        let mut cx = env.lx(b, rec.node);
        let (p, null) = crate::rep::datum(&mut cx, c, &rec.tfs[i]);
        env.memo = cx.memo;
        let p = b.select(null, env.zero, p);
        let Some(h) = h.filter(|_| copy) else {
            kept.push_in(p); // a float's row word (L0-m)
            continue;
        };
        if rec.bare >> i & 1 == 1 {
            // textkeys step 3 (road key.canon): a value of at most 15 bytes keeps no copy (0: the emit renders it from
            // the key's canonical image, `key_cell`); a longer one, or one whose header is not inline, is copied
            fires::fire("key.canon.bare"); // textkeys witness signal: a key kept with no copy of its inline values
            let (l_hdr, l_call, l_kept) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]));
            let isnull = b.icmp(IntCond::Eq, p, env.zero);
            b.cond_br(isnull, (l_kept, &[env.zero]), (l_hdr, &[]));
            b.switch_to(l_hdr);
            let small = crate::rep::varlena::key_canon::small(b, p);
            b.cond_br(small, (l_kept, &[env.zero]), (l_call, &[]));
            b.switch_to(l_call);
            let w = b.call_host(h, &[env.ctx, images, p], Some(Type::I64)).expect("returns the copy");
            let failed = b.icmp(IntCond::Slt, w, env.zero);
            let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
            let l_ok = b.create_block(&[]);
            b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
            b.switch_to(l_ok);
            b.br(l_kept, &[w]);
            b.switch_to(l_kept);
            kept.push_in(b.block_params(l_kept)[0]);
            continue;
        }
        let w = match fixed {
            Some(n) => {
                let hf = b.declare_host_once("key_copy_fixed", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64], Some(Type::I64), crate::table::key_copy::key_copy_fixed as *const () as usize, HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() });
                let len = b.iconst(Type::I64, n as i64);
                b.call_host(hf, &[env.ctx, images, p, len], Some(Type::I64))
            }
            None => b.call_host(h, &[env.ctx, images, p], Some(Type::I64)),
        }
        .expect("returns the copy");
        let (failed, l_ok) = (b.icmp(IntCond::Slt, w, env.zero), b.create_block(&[]));
        let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
        b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
        b.switch_to(l_ok);
        kept.push_in(w);
    }
    env.memo.restore(memo);
    b.br(l_join, &kept[..]);
    b.switch_to(l_join);
    for (j, &i) in keyed.iter().enumerate() {
        reps[i] = Some(b.block_params(l_join)[j]);
    }
    reps
}

/// Whether key `i` keeps a copy of its value (a varlena key with no code, a numeric: L0-l / L0-m), not its row word (a float).
pub(crate) fn copies(rec: &GroupRec<'_, '_>, i: usize) -> bool {
    rec.reps[i].is_some_and(|r| r.datum || r.nocode) && !rec.g.keys[i].image.is_float()
}

/// D3312: the record slots of `rec` holding a key copy's address (`copies`: the rep slot `open_rec` writes; a jsonb path
/// key's tagged, abi `IMG_SLOT_TAGGED`) as its table's copy-slot word (abi `IMG_SLOTS_KNOWN`, kernels `agg_copies`), which an
/// in-memory compact's reclaim reads. 0 (nothing reclaimed) where none does, where the arena holds copies no key slot names
/// (a text min / max, a keyed numeric extreme: `GroupRec::images`' other users; a hosted state), or where a key's slot is
/// past `IMG_SLOTS_MAX` or moved by `rep_slot`.
pub(crate) fn copy_slots(rec: &GroupRec<'_, '_>) -> u64 {
    use gravity_abi::breaker::{IMG_SLOTS_KNOWN, IMG_SLOTS_MAX, IMG_SLOT_PLAIN, IMG_SLOT_TAGGED};
    let others = rec.handle.is_some() || rec.aggs.iter().any(|a| crate::ops::shared::agg::order_keyed(&a.d) || crate::props::class_kind(&a.d) == KindId::ClassMinmaxCode);
    let mut mask = 0u64;
    for i in (0..rec.reps.len()).filter(|&i| copies(rec, i)) {
        let slot = rec.g.keys[i].rep;
        if others || slot as usize >= IMG_SLOTS_MAX || crate::frame::pairs_view::rep_slot(rec.g, slot) != slot {
            return 0;
        }
        mask |= if rec.reps[i].is_some_and(|r| r.nocode) { IMG_SLOT_TAGGED } else { IMG_SLOT_PLAIN } << (2 * slot);
    }
    if mask == 0 { 0 } else { mask | IMG_SLOTS_KNOWN }
}

/// Whether key `i` sorts by its image (M6 L0-m: a float / numeric key, whose kept datum is only its output) through the
/// word order arm, or (M6-TYPES) a uuid / interval / timetz key's two image words, signed in the type's order; a numeric's
/// order arm (not the word arm) refuses by its name at `node`.
///
/// M6-HOSTED SK_CMP (D1165): an imaged key whose image has no order but whose record keeps a copy (a numeric no packed
/// lane holds: N1 / N2) sorts by its copy under its type's comparator (false here: keyed/key_copy.rs `sorts_by_copy`;
/// the sort words' SK_CMP arm checks the comparator, runs/copy_words.rs `cmp_word`).
pub(crate) fn sorts_by_image(rec: &GroupRec<'_, '_>, i: usize, node: i32) -> bool {
    let (kd, imaged) = (&rec.g.keys[i], rec.reps[i].is_some_and(|r| r.imaged));
    let arm = crate::rep::key_kind(kd.kind, Some(kd.image), false, crate::rep::Accessor::Order);
    if imaged && arm != KindId::RepWordOrder && arm != KindId::RepFixedOrder {
        if copies(rec, i) {
            return false;
        }
        crate::rep::hosted_sorted(node, arm);
    }
    imaged
}

/// C10 2b (D3582): whether `rec`'s records hold no key copy, so its table spills by partition (`resident` keeps one whole).
pub(crate) fn spills(rec: &GroupRec<'_, '_>) -> bool {
    !(0..rec.reps.len()).any(|i| copies(rec, i))
}

/// The table's breaker never spills while its records hold key copies: it grows its grant (D6; L0-j carries them).
pub(crate) fn resident(rec: &GroupRec<'_, '_>, d: Pipeline) -> Pipeline {
    if (0..rec.reps.len()).any(|i| copies(rec, i)) { crate::frame::iface::end_resident(d, gravity_program::spill::NoSpill::KeyCopies) } else { d }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod d1392_tests {
    use gravity_abi::ctx::Ctx;

    use crate::table::key_copy::{key_copy, key_copy_fixed};

    /// D1392 (b): a ctx whose program binds no pin (`gravity_abi::program::pin` answers null, as a refused pool does).
    fn refused_pin_ctx() -> (Vec<u64>, Box<gravity_abi::ProgramBlock>) {
        let mut program = Box::new(gravity_abi::ProgramBlock::NONE);
        let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(1) / 8];
        // SAFETY: the words are a ctx image; `program` outlives every use.
        unsafe { (*(ctx.as_mut_ptr() as *mut Ctx)).program = &mut *program };
        (ctx, program)
    }

    /// The stashed error is PostgreSQL's "out of memory" (ERRCODE_OUT_OF_MEMORY), never nothing (the client's "an error
    /// without a message (raise code 7)").
    fn assert_out_of_memory() {
        let e = gravity_fn::take_error().expect("a refused pin stashes its error for the HOST raise");
        assert_eq!((e.message(), e.sqlstate()), ("out of memory", types_error::ERRCODE_OUT_OF_MEMORY));
        assert!(e.detail().is_some_and(|d| d.contains("pinned region")), "{:?}", e.detail());
    }

    /// A 4-byte-header varlena of `s`, 8-aligned.
    fn varlena(s: &[u8]) -> Vec<u64> {
        let mut v = vec![0u64; (4 + s.len()).div_ceil(8)];
        let p = v.as_mut_ptr() as *mut u8;
        // SAFETY: `v` holds 4 + s.len() bytes.
        unsafe {
            (p as *mut u32).write_unaligned(((4 + s.len()) as u32) << 2);
            core::ptr::copy_nonoverlapping(s.as_ptr(), p.add(4), s.len());
        }
        v
    }

    #[test]
    fn a_kept_key_copy_the_pool_refuses_names_out_of_memory() {
        let (mut ctx, _program) = refused_pin_ctx();
        let (text, mut images) = (varlena(b"MAIL"), [0u64; 3]);
        drop(gravity_fn::take_error());
        // SAFETY: a bound ctx; `text` a live varlena; `images` the three cursor words.
        let r = unsafe { key_copy((ctx.as_mut_ptr() as *mut Ctx) as u64, images.as_mut_ptr() as u64, text.as_ptr() as u64) };
        assert!(r < 0, "no room: the raise exit");
        assert_out_of_memory();
    }

    #[test]
    fn a_kept_fixed_key_copy_the_pool_refuses_names_out_of_memory() {
        let (mut ctx, _program) = refused_pin_ctx();
        let (uuid, mut images) = ([7u8; 16], [0u64; 3]);
        drop(gravity_fn::take_error());
        // SAFETY: as above; `uuid` 16 readable bytes.
        let r = unsafe { key_copy_fixed((ctx.as_mut_ptr() as *mut Ctx) as u64, images.as_mut_ptr() as u64, uuid.as_ptr() as u64, 16) };
        assert!(r < 0, "no room: the raise exit");
        assert_out_of_memory();
    }

    /// D2239: an inline compressed key datum (`VARATT_IS_4B_C`) is kept as it is, as PostgreSQL's `datumCopy` keeps it,
    /// never detoasted into the table's arena (its readers detoast it per use); a plain one keeps its payload.
    #[test]
    fn a_compressed_key_is_kept_compressed() {
        let mut arena = vec![0u64; 64];
        // SAFETY: `arena` holds 512 writable bytes; D3312: a chunk laid as a pin's (its header and start map first)
        let mut images = unsafe { gravity_abi::breaker::img_lay(arena.as_mut_ptr() as *mut u8, 64 * 8) };
        let base = images[0];
        // header: size 20 (4 header + 4 tcinfo + 12 compressed bytes) << 2 | 2; tcinfo: raw size 4800; bytes no pglz reads
        let mut packed = vec![0u64; 3];
        let p = packed.as_mut_ptr() as *mut u8;
        // SAFETY: `packed` holds 24 bytes.
        unsafe {
            (p as *mut u32).write_unaligned(20 << 2 | 2);
            (p.add(4) as *mut u32).write_unaligned(4800);
            core::ptr::copy_nonoverlapping(b"no pglz here".as_ptr(), p.add(8), 12);
        }
        let (mut ctx, _program) = refused_pin_ctx();
        // SAFETY: a bound ctx; `packed` a 20-byte compressed varlena image; `images` three live words.
        let r = unsafe { key_copy((ctx.as_mut_ptr() as *mut Ctx) as u64, images.as_mut_ptr() as u64, p as u64) };
        assert_eq!(r as u64, base, "copied into the arena, no detoast (its bytes are no pglz stream)");
        assert_eq!(images[0], base + 24, "20 bytes, 8-aligned");
        // SAFETY: both hold at least 20 bytes.
        assert_eq!(unsafe { core::slice::from_raw_parts(base as *const u8, 20) }, unsafe { core::slice::from_raw_parts(p as *const u8, 20) });
    }

    /// D3312: once a table's sparse evictions armed its dedupe, a re-opened group's copy takes the image its bytes already
    /// have (the same address, the arena's cursor unmoved); other bytes copy as before, and before arming every copy is new.
    #[test]
    fn a_tables_armed_dedupe_answers_a_copy_with_the_image_its_bytes_have() {
        use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, IMG_BY_REF, T1, T1_DEDUPE_ARM};
        use gravity_abi::hash::KeyHash;
        /// The test's `Program.pin`: a zeroed region kept to the process's end.
        unsafe extern "C" fn leak_pin(_ctx: *mut Ctx, bytes: u64) -> *mut u8 {
            Box::leak(vec![0u64; (bytes as usize).div_ceil(8)].into_boxed_slice()).as_mut_ptr() as *mut u8 // alloc: Cold
        }
        let (mut ctx, mut program) = refused_pin_ctx();
        program.pin = Some(leak_pin);
        let ctx = ctx.as_mut_ptr() as *mut Ctx;
        let l = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);
        let mut region = vec![0u64; (1 << 20) / 8]; // alloc: Cold
        // SAFETY: `region` is 1 MiB of zeroed, 8-aligned words.
        let t = unsafe { T1::at(region.as_mut_ptr() as *mut u8, &l, 3, 64 << 10, 1 << 20) } as *mut T1;
        // the claim's state words, as kernels `agg_open` lays them
        // SAFETY: `t` is the live table just laid out.
        let mut state = [unsafe { T1::img_words(t) } as u64, t as u64, IMG_BY_REF];
        let (mail, ship, uuid) = (varlena(b"MAIL"), varlena(b"SHIP"), [7u8; 16]);
        // SAFETY: a bound ctx; the data live varlenas / 16 readable bytes; `state` names the table's words.
        let var = |state: &mut [u64; 3], d: &[u64]| unsafe { key_copy(ctx as u64, state.as_mut_ptr() as u64, d.as_ptr() as u64) };
        let fixed = |state: &mut [u64; 3]| unsafe { key_copy_fixed(ctx as u64, state.as_mut_ptr() as u64, uuid.as_ptr() as u64, 16) };
        let (a, b) = (var(&mut state, &mail), var(&mut state, &mail));
        assert!(a > 0 && b > 0 && a != b, "not armed: each copy is new");
        // one key opened at its home and evicted again from a table under a quarter full, T1_DEDUPE_ARM times
        let keys = [1u64];
        // SAFETY: the live table; its home slot is empty before each open and holds the record each evict takes.
        unsafe {
            let home = (*t).home(KeyHash::of(&keys));
            for _ in 0..T1_DEDUPE_ARM {
                T1::open_at(t, home, &keys);
                assert!(T1::evict(t, home));
            }
        }
        let c1 = var(&mut state, &mail);
        // SAFETY: the live table's image words.
        let cur = unsafe { *T1::img_words(t) };
        assert_eq!(var(&mut state, &mail), c1, "armed: the same bytes take the image the first copy noted");
        assert_eq!(unsafe { *T1::img_words(t) }, cur, "no new copy");
        assert!(![a, b, c1].contains(&var(&mut state, &ship)), "other bytes copy anew");
        let f1 = fixed(&mut state);
        assert_eq!(fixed(&mut state), f1, "a fixed image dedupes the same way");
        // SAFETY: `c1` and `f1` are live images of 8 and 16 bytes.
        assert_eq!(unsafe { core::slice::from_raw_parts(c1 as *const u8, 8) }, unsafe { core::slice::from_raw_parts(mail.as_ptr() as *const u8, 8) });
        assert_eq!(unsafe { core::slice::from_raw_parts(f1 as *const u8, 16) }, &uuid);
        assert_eq!(state, [unsafe { T1::img_words(t) } as u64, t as u64, IMG_BY_REF], "the state's words stay the reference");
    }

    /// D1392 (a): a claim's state words that name the table's by reference (`[T1 words, 0, IMG_BY_REF]`) are resolved: the
    /// copy bumps the table's cursor, the state's words stay the reference, so the next claim's state keeps the same arena.
    #[test]
    fn a_state_naming_its_tables_words_copies_into_the_tables_arena() {
        let mut arena = vec![0u64; 64];
        // the table's (cur, end, chunk): room for the copies, no pin. SAFETY: `arena` holds 512 writable bytes
        let mut table = unsafe { gravity_abi::breaker::img_lay(arena.as_mut_ptr() as *mut u8, 64 * 8) };
        let base = table[0];
        let mut state = [table.as_mut_ptr() as u64, 0, gravity_abi::breaker::IMG_BY_REF];
        let (text, uuid) = (varlena(b"MAIL"), [7u8; 16]);
        let (mut ctx, _program) = refused_pin_ctx(); // a pin would be refused: the copies must bump the table's words
        // SAFETY: a bound ctx; `text` a live varlena, `uuid` 16 readable bytes; `state` names `table`'s three live words.
        let (a, b) = unsafe {
            (key_copy((ctx.as_mut_ptr() as *mut Ctx) as u64, state.as_mut_ptr() as u64, text.as_ptr() as u64),
             key_copy_fixed((ctx.as_mut_ptr() as *mut Ctx) as u64, state.as_mut_ptr() as u64, uuid.as_ptr() as u64, 16))
        };
        assert_eq!((a as u64, b as u64), (base, base + 8), "both copies come from the table's arena, in order");
        assert_eq!(table[0], base + 24, "the table's cursor moved past both copies (8 + 16 bytes)");
        assert_eq!(state, [table.as_ptr() as u64, 0, gravity_abi::breaker::IMG_BY_REF], "the state's words stay the reference");
    }
}
