//! Small-domain dense word table (serial). Hooks / levers: H23c R10 lower. Budget 184 lines (files.tsv, +25 % gate; V4R-368 +24).
//! Owner M1-WRITE (from M1-TABLES, 09-24).
//!
//! A one-word integer key over a small known domain indexes T1's own slot array: key `k`'s record is slot `k - lo`, with
//! no hash probe, eviction or observation (R10: T1's eviction protocol costs a 23-group W0 fold 188 ms, the lab 26.5).
//! - The domain `[lo, hi]` is `Domain::Const` or `Domain::Bound` (DenseLo / DenseHi Facts words, `(1, 0)` = none), over the
//!   key's integer image (sign-extended). The hashed table (bypass.rs) opens at `span` groups; the open goes dense when T1's
//!   slots cover the span and the table did not open bypassed, else every row takes the hashed table.
//! - Dense, a key with `k - lo < cap` (unsigned) takes its slot; a key past the slots (stale stats) is appended to its
//!   radix partition and the owner merges it with its slot's record: a wrong domain costs speed, never a row.
//! - V4R-368 (C12): one NULLABLE integer key over a bound domain, its image `(value, flags)`: NULL takes slot `span`, the
//!   one after the domain; a non-NULL key is in the slots only below `span` (past it: appended, never in NULL's slot).
//! - A fresh slot is listed as T1's drain reads it: `len` counted, its partition in `parts_of` (bypass.rs's route, the
//!   runtime's `Rec::route`, D250), `opened = OPENED_OVER` (re-set per fresh slot: an empty table's drain resets it).
//!
//! Registers: road `table.small` (`TABLE_SMALL`) as a `TableImpl`, the resolver's third policy.
use core::mem::offset_of;

use gravity_abi::breaker::{OPENED_OVER, T1, t1_default_bytes};
use gravity_expr::emit::hash::{emit_part, emit_route_and_full, Bits};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_kernels::host_abi::{AggState, AGG_BYPASS_WORD};

use crate::table::bypass::{open_record, state, HASHED};
use crate::table::face::{Domain, TableDecl, TableImpl, TableMech};
use crate::table::keyimg::ImageWords;
use crate::frame::env::Env;
use crate::frame::stop::Exits;

crate::strategies! {
    /// Small-domain dense word table (serial).
    TABLE_SMALL = 37, "table.small", Keyed;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(TABLE_SMALL, &TableImpl(&DENSE))], ..crate::ops::NONE };

pub(crate) struct Dense;
pub(crate) static DENSE: Dense = Dense;

const ST_T1: i32 = offset_of!(AggState, t) as i32;
const ST_TABLE: i32 = offset_of!(AggState, table) as i32;
const ST_PBITS: i32 = offset_of!(AggState, pbits) as i32;
const T1_CAP: i32 = offset_of!(T1, cap) as i32;
const T1_LEN: i32 = offset_of!(T1, len) as i32;
const T1_PARTS_OF: i32 = offset_of!(T1, hash_off) as i32;
const T1_OPENED: i32 = offset_of!(T1, opened) as i32;
const I64: Type = Type::I64;

impl Dense {
    /// This policy's two words after the hashed table's: the dense flag, then `lo`.
    fn words(&self, b: &mut FunctionBuilder<'_>, env: &Env, t: &TableDecl) -> Value {
        let st = state(b, env, t);
        b.ptr_add(st, env.zero, 1, HASHED.state_bytes(t) as i32)
    }

    /// `(lo, span)`: span 0 when the domain is none (`hi < lo`); `hi - lo + 1` modulo 2^64 (the whole range wraps to 0).
    fn domain(&self, b: &mut FunctionBuilder<'_>, env: &Env, t: &TableDecl) -> (Value, Value) {
        let (lo, hi) = match t.key.domain {
            Domain::Const { lo, hi } => (b.iconst(I64, lo), b.iconst(I64, hi)),
            _ => crate::table::size::domain(b, env, t),
        };
        let d = b.int_bin(IntBinOp::Sub, hi, lo);
        let span = b.int_bin(IntBinOp::Add, d, env.one);
        let none = b.icmp(IntCond::Slt, hi, lo);
        (lo, b.select(none, env.zero, span))
    }

    /// The dense form's slot of `img`: `(record, in_slots)`, the record's address only meaningful when in the slots.
    fn slot(&self, b: &mut FunctionBuilder<'_>, env: &Env, t: &TableDecl, img: &ImageWords) -> (Value, Value, Value) {
        let w = self.words(b, env, t);
        let lo = b.load(I64, w, 8);
        let st = state(b, env, t);
        let t1 = b.load(Type::Ptr, st, ST_T1);
        let cap = b.load_zext(Type::I32, t1, T1_CAP);
        let mut idx = b.int_bin(IntBinOp::Sub, img.words.words[0], lo);
        let mut lim = cap;
        if nullable(t) {
            // V4R-368: a NULL key (flags word set) takes slot `span`, the one after the domain's; a key past the domain
            // (stale bounds) appends even inside the slots, so it never lands in the NULL key's slot
            let (null, span) = (b.icmp(IntCond::Ne, img.words.words[1], env.zero), b.load(I64, w, 16));
            idx = b.select(null, span, idx);
            lim = b.select(null, cap, span);
        }
        let inside = b.icmp(IntCond::Ult, idx, lim);
        let table = b.load(Type::Ptr, st, ST_TABLE);
        let size = b.iconst(I64, t.layout.size() as i64);
        let off = b.int_bin(IntBinOp::Mul, idx, size);
        (b.ptr_add(table, off, 1, 0), inside, idx)
    }

    fn dense_on(&self, b: &mut FunctionBuilder<'_>, env: &Env, t: &TableDecl) -> Value {
        let w = self.words(b, env, t);
        let dense = b.load(I64, w, 0);
        b.icmp(IntCond::Ne, dense, env.zero)
    }
}

/// V4R-368 (C12): one nullable integer key over a bound domain, its image `(value, flags)` (emit_image); NULL at slot `span`.
fn nullable(t: &TableDecl) -> bool {
    t.image_words == 2 && t.layout.nkeys() == 2 && matches!(t.key.domain, Domain::Bound)
}

impl TableMech for Dense {
    fn upsert(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, h: Value) -> (Value, Value) {
        let (zero, words) = (env.zero, &img.words.words);
        let l_out = b.create_block(&[Type::Ptr, Type::Bool]);
        let (l_dense, l_hashed, l_in, l_app, l_new) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        let on = self.dense_on(b, env, t);
        b.cond_br(on, (l_dense, &[]), (l_hashed, &[]));
        b.switch_to(l_hashed);
        let (p, fresh) = HASHED.upsert(b, env, t, img, h);
        b.br(l_out, &[p, fresh]);
        b.switch_to(l_dense);
        let (p, inside, idx) = self.slot(b, env, t, img);
        b.cond_br(inside, (l_in, &[]), (l_app, &[]));
        b.switch_to(l_app);
        let st = state(b, env, t);
        let q = HASHED.append(b, env, t, st, img, h);
        let yes = b.bconst(true);
        b.br(l_out, &[q, yes]);
        b.switch_to(l_in);
        let s = b.load(I64, p, t.layout.sentinel_off() as i32);
        let (fresh, old) = (b.icmp(IntCond::Eq, s, zero), b.bconst(false));
        b.cond_br(fresh, (l_new, &[]), (l_out, &[p, old]));
        // a fresh slot: opened, counted and listed with its partition for the drain
        b.switch_to(l_new);
        open_record(b, p, &t.layout, words);
        let st = state(b, env, t);
        let t1 = b.load(Type::Ptr, st, ST_T1);
        let len = b.load_zext(Type::I32, t1, T1_LEN);
        let len = b.int_bin(IntBinOp::Add, len, env.one);
        let len = b.conv(ConvOp::Trunc, Type::I32, len);
        b.store(t1, T1_LEN, len);
        let pk = t.layout.partition_keys();
        let route = if pk >= words.len() { h } else { emit_route_and_full(b, &img.words, pk).0 };
        let pbits = b.load(I64, st, ST_PBITS);
        let part = emit_part(b, route, Bits::Dyn(pbits));
        let part = b.conv(ConvOp::Trunc, Type::I32, part);
        let parts_off = b.load_zext(Type::I32, t1, T1_PARTS_OF);
        let parts = b.ptr_add(t1, parts_off, 1, 0);
        let at = b.ptr_add(parts, idx, 4, 0);
        b.store(at, 0, part);
        let over = b.const_raw(Type::I32, OPENED_OVER as u64);
        b.store(t1, T1_OPENED, over);
        let yes = b.bconst(true);
        b.br(l_out, &[p, yes]);
        b.switch_to(l_out);
        (b.block_params(l_out)[0], b.block_params(l_out)[1])
    }

    fn find(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, h: Value) -> Value {
        let l_out = b.create_block(&[Type::Ptr]);
        let (l_dense, l_hashed, l_in) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        let null = b.conv(ConvOp::IntToPtr, Type::Ptr, env.zero);
        let on = self.dense_on(b, env, t);
        b.cond_br(on, (l_dense, &[]), (l_hashed, &[]));
        b.switch_to(l_hashed);
        let p = HASHED.find(b, env, t, img, h);
        b.br(l_out, &[p]);
        b.switch_to(l_dense);
        let (p, inside, _) = self.slot(b, env, t, img);
        b.cond_br(inside, (l_in, &[]), (l_out, &[null]));
        b.switch_to(l_in);
        let s = b.load(I64, p, t.layout.sentinel_off() as i32);
        let used = b.icmp(IntCond::Ne, s, env.zero);
        let r = b.select(used, p, null);
        b.br(l_out, &[r]);
        b.switch_to(l_out);
        b.block_params(l_out)[0]
    }

    fn state_bytes(&self, t: &TableDecl) -> u32 {
        HASHED.state_bytes(t) + if nullable(t) { 24 } else { 16 }
    }

    /// The claim's open at the domain's span (else the size bound); dense when T1's slots cover the span and the open did
    /// not bypass the table (a resumed bypassed table keeps the hashed form).
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl) {
        let (lo, span) = self.domain(b, env, t);
        let bound = crate::table::size::groups(b, env, t);
        let none = b.icmp(IntCond::Eq, span, env.zero);
        let n = match nullable(t) {
            true => {
                let n = b.int_bin(IntBinOp::Add, span, env.one);
                b.select(none, env.zero, n)
            }
            false => span,
        };
        let groups = b.select(none, bound, n);
        HASHED.open_with(b, env, t, groups);
        let st = state(b, env, t);
        let t1 = b.load(Type::Ptr, st, ST_T1);
        let cap = b.load_zext(Type::I32, t1, T1_CAP);
        let last = b.int_bin(IntBinOp::Sub, n, env.one);
        let fits = b.icmp(IntCond::Ult, last, cap);
        let bypass = b.load(I64, st, AGG_BYPASS_WORD as i32);
        let hashed = b.icmp(IntCond::Eq, bypass, env.zero);
        let flag = b.select(hashed, env.one, env.zero);
        let dense = b.select(fits, flag, env.zero);
        let w = self.words(b, env, t);
        b.store(w, 0, dense);
        b.store(w, 8, lo);
        if nullable(t) {
            b.store(w, 16, span);
        }
    }

    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, x: &Exits) {
        HASHED.finish(b, env, t, x)
    }

    /// One integer word over a known domain: a constant one within a full table's slots, or a bound one whose Facts words
    /// the maker declared (`keyed_table_size::declare_facts`).
    fn takes(&self, t: &TableDecl) -> bool {
        (t.image_words == 1 && t.layout.nkeys() == 1 || nullable(t))
            && match t.key.domain {
                Domain::Const { lo, hi } => hi >= lo && hi.abs_diff(lo) < (t1_default_bytes() / t.layout.size()) as u64,
                Domain::Bound => t.facts != 0,
                Domain::Unknown | Domain::Div { .. } | Domain::Shift { .. } => false,
            }
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use gravity_abi::breaker::{HashAgg, KeyKind, Rec, RecordLayout, SlotKind, OPENED_OVER, T1};
    use gravity_abi::ctx::{ctx_size, site_off};
    use gravity_abi::hash::{Depth, KeyHash};
    use gravity_abi::program::partial;
    use gravity_abi::site::site;
    use gravity_abi::{status, Ctx, Partials, ProgramBlock};
    use gravity_expr::emit::hash::{emit_key_hash, ImageValues};
    use gravity_interp::Interp;
    use gravity_ir::builder::FunctionBuilder;
    use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};

    use super::DENSE;
    use crate::face::end::EndMech;
    use crate::table::face::{Domain, KeyShape, TableDecl, TableMech};
    use crate::table::keyimg::ImageWords;
    use crate::face::source::SourceMech;
    use crate::frame::env::{self, Env};
    use crate::frame::frame::{Mechs, Win};
    use crate::frame::open::{Base, OpenWord, OpenWords};
    use crate::frame::row::{Carry, RowVals};
    use crate::frame::stop::Exits;

    const I64: Type = Type::I64;
    const SHARE: usize = 16 << 20;
    const LAYOUT: RecordLayout = RecordLayout::with_sentinel(&[KeyKind::I64], &[SlotKind::SumI64], false, None);

    /// Rows `[cursor, n)` in one window.
    struct Src(i64);

    impl SourceMech for Src {
        fn open_words(&self, w: &mut OpenWords) {
            w.declare(OpenWord::open("cursor", Base::Ctx, (site_off(0) + site::CURSOR) as i32, Type::I32, 0));
        }
        fn open(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Value {
            b.iconst(I64, 0)
        }
        fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
            let cursor = b.load_zext(Type::I32, env.ctx, (site_off(0) + site::CURSOR) as i32);
            let end = b.iconst(I64, self.0);
            let past = b.icmp(IntCond::Sge, cursor, end);
            let (l_end, l_win) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(past, (l_end, &[]), (l_win, &[]));
            b.switch_to(l_end);
            b.br(l_eof, &[env.zero]);
            b.switch_to(l_win);
            Win { cursor, end, facts: Default::default() }
        }
        fn row<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
            RowVals { row: crate::cell::Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry, next, at: j }
        }
    }

    /// Row `i` keyed by `base + i % g`: its record counted in the sentinel and `i` summed into slot 0. No finish: the test
    /// reads the table and the partitions as the claim left them.
    struct Fold(i64, i64, TableDecl);

    impl EndMech for Fold {
        fn state_bytes(&self) -> u32 {
            DENSE.state_bytes(&self.2)
        }
        fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
            DENSE.open(b, env, &self.2)
        }
        fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
            let (g, base) = (b.iconst(I64, self.1), b.iconst(I64, self.0));
            let r = b.int_bin(IntBinOp::Urem, row.at, g);
            let k = b.int_bin(IntBinOp::Add, r, base);
            let img = ImageWords::plain(ImageValues { words: vec![k] }, true);
            let h = emit_key_hash(b, &img.words);
            let (rec, _fresh) = DENSE.upsert(b, env, &self.2, &img, h);
            for (off, add) in [(LAYOUT.sentinel_off() as i32, env.one), (LAYOUT.slot_off(0) as i32, row.at)] {
                let v = b.load(I64, rec, off);
                let v = b.int_bin(IntBinOp::Add, v, add);
                b.store(rec, off, v);
            }
            Carry { out: row.carry.out, accs: mcx::PgVec::new_in(env.mcx) }
        }
    }

    fn decl(domain: Domain) -> TableDecl {
        TableDecl { image_words: 1, record_bytes: 16, expect: None, depth: Depth::TOP, life: gravity_program::LifeId(0), layout: LAYOUT, state_off: 0, key: KeyShape { code: None, dense: None, domain }, facts: 0, copies: 0 }
    }

    /// Folds rows `0..n` keyed by `base + i % g` over `[lo, hi]`; per key `(rows, sum)` found in its slot `k - lo` (none
    /// elsewhere in the table) and in the partition buffers, with the slots counted in `len`, listed in `parts_of` by the
    /// key's KeyHash partition and `opened = OPENED_OVER`. Returns (keys in slots, keys in the buffers).
    fn fold(n: i64, base: i64, g: i64, lo: i64, hi: i64) -> (usize, usize) {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let t = decl(Domain::Const { lo, hi });
        let desc = gravity_program::Pipeline::eligible(0, gravity_program::End::Sink, 0);
        let mut m = Mechs { k: 0, source: mcx::pg_box_dyn!(rig.mcx(), Src(n) => dyn SourceMech), steps: mcx::PgVec::new_in(rig.mcx()), end: mcx::pg_box_dyn!(rig.mcx(), Fold(base, g, t) => dyn EndMech), site: Some(0), desc, words: None, state_word: None };
        let mut pb = gravity_ir::builder::ProgramBuilder::new();
        let core = env::declare(&mut pb);
        let (f, _) = crate::frame::frame::pipeline_fn(&mut pb, core, crate::hir::Ir::none(), &[], "p0", 1, &mut m, rig.mcx());
        let p = pb.finish();
        gravity_ir::verify::verify(&p).expect("the dense table's IR verifies");
        let mut own = vec![0u64; SHARE / 8];
        unsafe { HashAgg::arm(own.as_mut_ptr() as *mut u8, SHARE as u32) };
        let mut partials = Box::new(Partials::two(own.as_mut_ptr() as *mut u8, [core::ptr::null_mut(); 2]));
        let mut program = Box::new(ProgramBlock::NONE);
        program.partial = &mut *partials;
        let (mut ctx, mut out) = (vec![0u64; ctx_size(1) / 8], [0u64; 32]);
        ctx[gravity_abi::ctx::hdr::OUT / 8] = out.as_mut_ptr() as u64;
        let c = ctx.as_mut_ptr() as *mut Ctx;
        unsafe { (*c).program = &mut *program };
        assert_eq!(Interp::new(&p).run(f, &[c as u64]).expect("runs")[0] as i64, status::DONE);
        let t1 = unsafe { T1::view(partial(c)) };
        let (mut got, mut want) = (vec![(0i64, 0i64); g as usize], vec![(0i64, 0i64); g as usize]);
        for i in 0..n {
            let e = &mut want[(i % g) as usize];
            *e = (e.0 + 1, e.1 + i);
        }
        let (mut slots, mut buffered) = (0, 0);
        let parts_of = unsafe { (t1 as *const T1 as *const u8).add(t1.hash_off as usize) as *const u32 };
        for i in 0..t1.cap as usize {
            let r = t1.record(i);
            if r.used() {
                let k = r.key(0) as i64;
                assert_eq!(k - lo, i as i64, "key {k} in its dense slot");
                assert_eq!(unsafe { *parts_of.add(i) }, KeyHash::of(r.keys()).part(t1.pbits as u32), "slot {i} listed in its key's partition");
                let e = &mut got[(k - base) as usize];
                *e = (e.0 + r.count(1) - 1, e.1 + r.i64(0));
                slots += 1;
            }
        }
        assert_eq!(t1.len as usize, slots, "every fresh slot counted");
        assert!(slots == 0 || t1.opened == OPENED_OVER, "the drain walks every slot");
        for part in 0..t1.parts() as u32 {
            for ch in t1.chain(part) {
                for j in 0..ch.recs {
                    let r = unsafe { Rec::at((ch.ptr as *mut u8).add(j * LAYOUT.size() as usize), &LAYOUT) };
                    assert_eq!(KeyHash::of(r.keys()).part(t1.pbits as u32), part, "key {} appended to its partition", r.key(0));
                    let e = &mut got[(r.key(0) as i64 - base) as usize];
                    *e = (e.0 + r.count(1) - 1, e.1 + r.i64(0));
                    buffered += 1;
                }
            }
        }
        assert!(got == want, "per-key rows and sums");
        (slots, buffered)
    }

    #[test]
    fn a_small_domain_folds_in_its_dense_slots() {
        assert_eq!(fold(10_000, 100, 23, 100, 122), (23, 0), "23 keys, 23 slots, nothing appended");
        // keys 90..99 fall below lo (appended, one record per row); 123..129 lie past hi but inside the slots
        assert_eq!(fold(4_000, 90, 40, 100, 122), (30, 1_000), "30 keys in slots, 10 keys x 100 rows appended");
    }

    #[test]
    fn takes_one_integer_word_over_a_known_domain() {
        let max = (gravity_abi::breaker::t1_default_bytes() / LAYOUT.size()) as i64;
        assert!(DENSE.takes(&decl(Domain::Const { lo: -5, hi: 17 })));
        assert!(!DENSE.takes(&decl(Domain::Const { lo: 0, hi: max })), "a domain past a full table's slots");
        assert!(!DENSE.takes(&decl(Domain::Const { lo: 3, hi: 2 })));
        assert!(!DENSE.takes(&decl(Domain::Unknown)));
        assert!(!DENSE.takes(&decl(Domain::Bound)), "a bound domain without its Facts words");
        let two = TableDecl { image_words: 2, layout: RecordLayout::with_sentinel(&[KeyKind::I64, KeyKind::I64], &[], false, None), ..decl(Domain::Const { lo: 0, hi: 9 }) };
        assert!(!DENSE.takes(&two));
    }

    const LAYOUT_N: RecordLayout = RecordLayout::with_sentinel(&[KeyKind::I64, KeyKind::I64], &[SlotKind::SumI64], false, None);

    /// V4R-368: row `i` keyed by `base + i % g`, its key NULL on every 7th row (the image `(0, 1)`: value zeroed, flags
    /// set, as emit_image), its record counted and `i` summed as `Fold`'s.
    struct FoldN(i64, i64, TableDecl);

    impl EndMech for FoldN {
        fn state_bytes(&self) -> u32 {
            DENSE.state_bytes(&self.2)
        }
        fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
            DENSE.open(b, env, &self.2)
        }
        fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
            let (g, base, seven) = (b.iconst(I64, self.1), b.iconst(I64, self.0), b.iconst(I64, 7));
            let r = b.int_bin(IntBinOp::Urem, row.at, g);
            let k = b.int_bin(IntBinOp::Add, r, base);
            let m = b.int_bin(IntBinOp::Urem, row.at, seven);
            let null = b.icmp(IntCond::Eq, m, env.zero);
            let (k, flags) = (b.select(null, env.zero, k), b.select(null, env.one, env.zero));
            let img = ImageWords::plain(ImageValues { words: vec![k, flags] }, true);
            let h = emit_key_hash(b, &img.words);
            let (rec, _fresh) = DENSE.upsert(b, env, &self.2, &img, h);
            for (off, add) in [(LAYOUT_N.sentinel_off() as i32, env.one), (LAYOUT_N.slot_off(0) as i32, row.at)] {
                let v = b.load(I64, rec, off);
                let v = b.int_bin(IntBinOp::Add, v, add);
                b.store(rec, off, v);
            }
            Carry { out: row.carry.out, accs: mcx::PgVec::new_in(env.mcx) }
        }
    }

    fn decl_n() -> TableDecl {
        TableDecl { image_words: 2, layout: LAYOUT_N, facts: 1, ..decl(Domain::Bound) }
    }

    /// Folds rows `0..n` (`FoldN`) over the bound domain `[lo, hi]` (Facts words DenseLo / DenseHi / TableSize in the
    /// params, TableSize the span + 1): per key and NULL `(rows, sum)`, over the slots and the partition buffers. A key in
    /// `[lo, hi]` sits in slot `k - lo`, NULL in slot `hi - lo + 1`, any other slot is empty. Returns (keys in slots, keys
    /// in the buffers, whether NULL sat in its slot).
    fn fold_n(n: i64, base: i64, g: i64, lo: i64, hi: i64) -> (usize, usize, bool) {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let desc = gravity_program::Pipeline::eligible(0, gravity_program::End::Sink, 0);
        let mut m = Mechs { k: 0, source: mcx::pg_box_dyn!(rig.mcx(), Src(n) => dyn SourceMech), steps: mcx::PgVec::new_in(rig.mcx()), end: mcx::pg_box_dyn!(rig.mcx(), FoldN(base, g, decl_n()) => dyn EndMech), site: Some(0), desc, words: None, state_word: None };
        let mut pb = gravity_ir::builder::ProgramBuilder::new();
        let core = env::declare(&mut pb);
        let (f, _) = crate::frame::frame::pipeline_fn(&mut pb, core, crate::hir::Ir::none(), &[], "p0", 1, &mut m, rig.mcx());
        let p = pb.finish();
        gravity_ir::verify::verify(&p).expect("the dense table's IR verifies");
        let mut own = vec![0u64; SHARE / 8];
        unsafe { HashAgg::arm(own.as_mut_ptr() as *mut u8, SHARE as u32) };
        let mut partials = Box::new(Partials::two(own.as_mut_ptr() as *mut u8, [core::ptr::null_mut(); 2]));
        let mut program = Box::new(ProgramBlock::NONE);
        program.partial = &mut *partials;
        let params = [0, lo as u64, hi as u64, (hi - lo + 2) as u64];
        let (mut ctx, mut out) = (vec![0u64; ctx_size(1) / 8], [0u64; 32]);
        ctx[gravity_abi::ctx::hdr::OUT / 8] = out.as_mut_ptr() as u64;
        ctx[gravity_abi::ctx::hdr::PARAMS / 8] = params.as_ptr() as u64;
        let c = ctx.as_mut_ptr() as *mut Ctx;
        unsafe { (*c).program = &mut *program };
        assert_eq!(Interp::new(&p).run(f, &[c as u64]).expect("runs")[0] as i64, status::DONE);
        let t1 = unsafe { T1::view(partial(c)) };
        // index g: the NULL key
        let (mut got, mut want) = (vec![(0i64, 0i64); g as usize + 1], vec![(0i64, 0i64); g as usize + 1]);
        for i in 0..n {
            let e = &mut want[if i % 7 == 0 { g as usize } else { (i % g) as usize }];
            *e = (e.0 + 1, e.1 + i);
        }
        let at = |r: &Rec| if r.key(1) != 0 { g as usize } else { (r.key(0) as i64 - base) as usize };
        let (mut slots, mut buffered, mut null_slot) = (0, 0, false);
        for i in 0..t1.cap as usize {
            let r = t1.record(i);
            if r.used() {
                if r.key(1) != 0 {
                    assert_eq!((i as i64, r.key(0)), (hi - lo + 1, 0), "NULL in the slot after the domain, its value zeroed");
                    null_slot = true;
                } else {
                    assert!((lo..=hi).contains(&(r.key(0) as i64)), "only a key within the domain takes a slot");
                    assert_eq!(r.key(0) as i64 - lo, i as i64, "key {} in its dense slot", r.key(0));
                }
                let e = &mut got[at(&r)];
                *e = (e.0 + r.count(1) - 1, e.1 + r.i64(0));
                slots += 1;
            }
        }
        assert_eq!(t1.len as usize, slots, "every fresh slot counted");
        for part in 0..t1.parts() as u32 {
            for ch in t1.chain(part) {
                for j in 0..ch.recs {
                    let r = unsafe { Rec::at((ch.ptr as *mut u8).add(j * LAYOUT_N.size() as usize), &LAYOUT_N) };
                    assert_eq!(r.key(1), 0, "NULL never appends");
                    let e = &mut got[at(&r)];
                    *e = (e.0 + r.count(1) - 1, e.1 + r.i64(0));
                    buffered += 1;
                }
            }
        }
        assert!(got == want, "per-key and NULL rows and sums");
        (slots, buffered, null_slot)
    }

    #[test]
    fn a_nullable_key_keeps_null_in_the_slot_after_its_bound_domain() {
        // 23 keys over [100, 122]: 23 slots + NULL's (slot 23), nothing appended
        assert_eq!(fold_n(10_000, 100, 23, 100, 122), (24, 0, true));
        // stale bounds [100, 119]: keys 120..122 lie past the domain INSIDE the slots, and 120 would take NULL's slot 20:
        // each appends (its rows less the NULL rows: 3 keys x ~372 records), NULL keeps slot 20
        let (slots, buffered, null_slot) = fold_n(10_000, 100, 23, 100, 119);
        assert_eq!((slots, null_slot), (21, true));
        assert_eq!(buffered, (0..10_000i64).filter(|i| i % 7 != 0 && i % 23 >= 20).count());
        // keys below lo (90..99) append too
        assert_eq!(fold_n(4_000, 90, 40, 100, 122).2, true);
        assert!(DENSE.takes(&decl_n()), "one nullable integer key over a bound domain with its Facts words");
        assert!(!DENSE.takes(&TableDecl { facts: 0, ..decl_n() }), "without its Facts words");
        assert!(!DENSE.takes(&TableDecl { key: KeyShape { code: None, dense: None, domain: Domain::Const { lo: 0, hi: 9 } }, ..decl_n() }), "a nullable key over a constant domain");
    }
}
