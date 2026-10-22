//! Per-part code front. Hooks / levers: H23 R3-L09. Budget 160 lines (files.tsv, +25 % gate). Owner M1-TABLES.
//!
//! A table keyed by one code key (`KeyShape.code`; the row's code is `ImageWords.code`) folds a window's rows into a
//! per-worker array of records indexed by the code, so each distinct code of the window costs one record and the hashed
//! table sees one merge per code, never a probe per row (the ladder's `cnt[code]` front, generalised to every slot kind).
//! A fresh code's record opens with the row's image (the Fp128 words every table keys the value by) and is listed; at
//! every window's end the listed records merge into the hashed table (`agg_front_flush`: `locate`, then `Rec::fold`, the
//! owner phase's merge at PG's transition-state widths; appended under the bypass) and are zeroed.
//!
//! Codes are part-local (each pgrc part has its own dictionary). A window reads one dictionary per lane (its table words
//! are the window's), so flushing at every window end means the front never holds two parts' records: this holds by
//! construction, not by a run-time check. A stop exit ends the window through `window_end` too (frame.carry).
//!
//! A NULL key (its code all ones), a code at or past the front's capacity, or a front the grant refused takes the hashed
//! table (`keyed_bypass`). `agg_front_open` returns null when the claim's arena, which draws from the statement's ledger
//! (W / G), refuses it.
//!
//! The run cache (`PGRUST_GRAVITY_FRONT_RUNS`, on unless 0): a front takes the table, so no run carry rides it
//! (distinct_plain.rs `run_carry`), and a code past the capacity paid a hashed upsert per row, under the bypass an
//! appended record per row. The cache keeps the last code that took the table and its record (`RunWords`), and a row of
//! that code folds into the record with no image, hash or probe: a run of one code in a dictionary past the capacity
//! costs one upsert, as under the run carry (a key whose rows sit together, a session's events, with far more distinct
//! codes than the front holds: without the cache the front ran ~4x the run carry's instructions). Only between two
//! upserts is the record valid: the cache is forgotten at the claim's open, at every window's end (the flush
//! folds into the table; the next window may read another part's dictionary) and at an upsert of a plain image.
//!
//! Hosts: agg_front_open, agg_front_flush (host_abi v4 m1).
//!
//! Registers: road `code.front_part` (`CODE_FRONT_PART`) as a `TableImpl` that takes a one-code-key table.
use gravity_abi::breaker::SlotKind;
use gravity_expr::emit::hash::emit_key_hash;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};

use crate::table::bypass::{fail_on, host, open_record, state, HASHED};
use crate::table::face::{TableDecl, TableImpl, TableMech};
use crate::table::keyimg::ImageWords;
use crate::frame::env::Env;
use crate::frame::stop::Exits;

crate::strategies! {
    /// Per-part code front.
    CODE_FRONT_PART = 46, "code.front_part", CodeKeyed;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(CODE_FRONT_PART, &TableImpl(&FRONT))], ..crate::ops::NONE };

pub(crate) struct Front;
pub(crate) static FRONT: Front = Front;

/// The front's bytes per worker: its records and the touched list (one word per code), L2-resident.
const FRONT_BYTES: u64 = 256 << 10;
/// A front's words after the hashed table's (this front's, and codefront_percode.rs's per-code front's): the base of its
/// region (the records, or the per-code front), the touched list, its length, the capacity in codes (0: off). The IR
/// reads and writes them at these offsets.
#[repr(C)]
pub(crate) struct FrontWords {
    base: u64,
    list: u64,
    n: u64,
    cap: u64,
}
pub(crate) const FR_BASE: i32 = core::mem::offset_of!(FrontWords, base) as i32;
pub(crate) const FR_LIST: i32 = core::mem::offset_of!(FrontWords, list) as i32;
pub(crate) const FR_N: i32 = core::mem::offset_of!(FrontWords, n) as i32;
pub(crate) const FR_CAP: i32 = core::mem::offset_of!(FrontWords, cap) as i32;
/// The front words' bytes.
pub(crate) const FR_WORDS: u32 = core::mem::size_of::<FrontWords>() as u32;
/// The run cache's words after the front's (code.front_part's alone, `runs()` on): the last code that took the hashed
/// table and its record there.
#[repr(C)]
struct RunWords {
    code: u64,
    rec: u64,
}
const RW_CODE: i32 = FR_WORDS as i32 + core::mem::offset_of!(RunWords, code) as i32;
const RW_REC: i32 = FR_WORDS as i32 + core::mem::offset_of!(RunWords, rec) as i32;
/// No row's code: a code indexes a part's dictionary (under 2^32), a NULL's is all ones.
const RUN_NONE: u64 = u64::MAX - 1;

/// The run cache's switch (`set_runs`): on until the executor sets it from `PGRUST_GRAVITY_FRONT_RUNS` where it registers
/// the lowerer (exec inputs/knobs/diag.rs); off, the front lowers as before (an A/B and rollback lever).
static RUNS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// Sets the run cache's switch for the process (exec, once, before the first lowering: dict/face.rs `set_front_runs`).
pub(crate) fn set_runs(on: bool) {
    RUNS.store(on, core::sync::atomic::Ordering::Relaxed);
}

fn runs() -> bool {
    RUNS.load(core::sync::atomic::Ordering::Relaxed)
}

/// The front's capacity in codes for `t`'s record (program cost/dict_front.rs `capacity`).
pub(crate) fn cap(t: &TableDecl) -> u64 {
    gravity_program::cost::dict_front::capacity(FRONT_BYTES, t.layout.size() as u64)
}

/// The state's address and the front words' offset in it.
fn words(b: &mut FunctionBuilder<'_>, env: &Env, t: &TableDecl) -> (Value, i32) {
    (state(b, env, t), HASHED.state_bytes(t) as i32)
}

impl Front {
    /// `l_front(record)` when the row's code is under the front's capacity, else `l_hash` (a NULL key's code is all ones:
    /// `ImageWords.code`).
    pub(crate) fn route(&self, b: &mut FunctionBuilder<'_>, env: &Env, t: &TableDecl, code: Value) -> (gravity_ir::Block, gravity_ir::Block) {
        let (st, fr) = words(b, env, t);
        let (l_front, l_hash) = (b.create_block(&[Type::Ptr]), b.create_block(&[]));
        let cap = b.load(Type::I64, st, fr + FR_CAP);
        let ok = b.icmp(IntCond::Ult, code, cap);
        let l_at = b.create_block(&[]);
        b.cond_br(ok, (l_at, &[]), (l_hash, &[]));
        b.switch_to(l_at);
        let base = b.load(Type::Ptr, st, fr + FR_BASE);
        let size = b.iconst(Type::I64, t.layout.size() as i64);
        let off = b.int_bin(IntBinOp::Mul, code, size);
        let rec = b.ptr_add(base, off, 1, 0);
        b.br(l_front, &[rec]);
        (l_front, l_hash)
    }

    /// The hashed table's open, then the front: records and list from the claim's arena (zeroed), the list empty; a
    /// refused front has capacity 0, so every row takes the table. Shared with code.front_dense, so it fires no road.
    pub(crate) fn open_front(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl) {
        HASHED.open(b, env, t);
        let (st, fr) = words(b, env, t);
        let (n, rec) = (cap(t), t.layout.size() as u64);
        let bytes = b.iconst(Type::I64, (n * (rec + 8)) as i64);
        let f = host(b, "agg_front_open", &[Type::Ptr, Type::I64], gravity_abi::host_words!(gravity_kernels::table::entries::agg_front_open(ctx, bytes)));
        let p = b.call_host(f, &[env.ctx, bytes], Some(Type::I64)).expect("agg_front_open returns an address or 0");
        let refused = b.icmp(IntCond::Eq, p, env.zero);
        let full = b.iconst(Type::I64, n as i64);
        let capv = b.select(refused, env.zero, full);
        let base = b.conv(ConvOp::IntToPtr, Type::Ptr, p);
        let list = b.ptr_add(base, env.zero, 1, (n * rec) as i32);
        b.store(st, fr + FR_BASE, base);
        b.store(st, fr + FR_LIST, list);
        b.store(st, fr + FR_N, env.zero);
        b.store(st, fr + FR_CAP, capv);
        self.forget(b, env, t);
    }

    /// The run cache emptied (`RunWords`).
    fn forget(&self, b: &mut FunctionBuilder<'_>, env: &Env, t: &TableDecl) {
        if runs() {
            let (st, fr) = words(b, env, t);
            let none = b.const_raw(Type::I64, RUN_NONE);
            b.store(st, fr + RW_CODE, none);
        }
    }

    /// The listed records merged into the hashed table and zeroed; the list emptied; the run cache forgotten.
    pub(crate) fn flush(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, x: &Exits) {
        self.forget(b, env, t);
        let (st, fr) = words(b, env, t);
        let n = b.load(Type::I64, st, fr + FR_N);
        let (l_go, l_done) = (b.create_block(&[]), b.create_block(&[]));
        let any = b.icmp(IntCond::Ne, n, env.zero);
        b.cond_br(any, (l_go, &[]), (l_done, &[]));
        b.switch_to(l_go);
        let (base, list) = (b.load(Type::Ptr, st, fr + FR_BASE), b.load(Type::Ptr, st, fr + FR_LIST));
        let f = host(b, "agg_front_flush", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::Ptr, Type::I64], gravity_abi::host_words!(gravity_kernels::table::entries::agg_front_flush(ctx, st, front, touched, n)));
        let r = b.call_host(f, &[env.ctx, st, base, list, n], Some(Type::I64)).expect("agg_front_flush returns a status");
        fail_on(b, env, x, r);
        b.store(st, fr + FR_N, env.zero);
        b.br(l_done, &[]);
        b.switch_to(l_done);
    }

    /// The record of front index `code` (a code, or code.front_dense's pair index), opened and listed when fresh; past the
    /// capacity the hashed table's, with the hash emitted in that arm alone (the caller's per-row hash stays dead), or with
    /// `run` (`code` is the row's exact code: code.front_part's) the run cache's when it holds `code`; under
    /// `Env::front_probed` the probe compared already and missed, so the arm only refills the cache.
    /// D3224 A2: `Env::rec_window` names the arm (true: the front's record, one window's rows).
    pub(crate) fn upsert_at(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, code: Value, run: bool) -> (Value, Value) {
        let run = run && runs();
        let check = run && !env.front_probed;
        let l_out = b.create_block(&[Type::Ptr, Type::Bool, Type::Bool]);
        let (l_front, l_hash) = self.route(b, env, t, code);
        b.switch_to(l_front);
        let rec = b.block_params(l_front)[0];
        let c = b.load(Type::I64, rec, t.layout.sentinel_off() as i32);
        let (l_open, l_have) = (b.create_block(&[]), b.create_block(&[]));
        let fresh = b.icmp(IntCond::Eq, c, env.zero);
        b.cond_br(fresh, (l_open, &[]), (l_have, &[]));
        b.switch_to(l_open);
        open_record(b, rec, &t.layout, &img.words.words);
        let (st, fr) = words(b, env, t);
        let (list, n) = (b.load(Type::Ptr, st, fr + FR_LIST), b.load(Type::I64, st, fr + FR_N));
        let at = b.ptr_add(list, n, 8, 0);
        b.store(at, 0, code);
        let n1 = b.int_bin(IntBinOp::Add, n, env.one);
        b.store(st, fr + FR_N, n1);
        let yes = b.bconst(true);
        b.br(l_out, &[rec, yes, yes]);
        b.switch_to(l_have);
        let (no, yes) = (b.bconst(false), b.bconst(true));
        b.br(l_out, &[rec, no, yes]);
        b.switch_to(l_hash);
        let no = b.bconst(false);
        if check {
            let (st, fr) = words(b, env, t);
            let last = b.load(Type::I64, st, fr + RW_CODE);
            let (same, l_hit, l_new) = (b.icmp(IntCond::Eq, code, last), b.create_block(&[]), b.create_block(&[]));
            b.cond_br(same, (l_hit, &[]), (l_new, &[]));
            b.switch_to(l_hit);
            let r = b.load(Type::Ptr, st, fr + RW_REC);
            b.br(l_out, &[r, no, no]);
            b.switch_to(l_new);
        }
        let h = emit_key_hash(b, &img.words);
        let (r, f) = HASHED.upsert(b, env, t, img, h);
        if run {
            let (st, fr) = words(b, env, t);
            b.store(st, fr + RW_CODE, code);
            b.store(st, fr + RW_REC, r);
        }
        b.br(l_out, &[r, f, no]);
        b.switch_to(l_out);
        env.rec_window = Some(b.block_params(l_out)[2]);
        (b.block_params(l_out)[0], b.block_params(l_out)[1])
    }

    /// D3224 A4 (`TableMech::probe`): `l_hot(record, window)` when front index `code` holds an open record (`window` true)
    /// or, with `run` (`upsert_at`'s), the run cache holds `code` (the table's record, `window` false), else `l_cold` (a
    /// fresh record, a code past the capacity), the builder left in no block. Per row: the route's compare and the count's
    /// load.
    pub(crate) fn probe_at(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, code: Value, run: bool) -> (gravity_ir::Block, gravity_ir::Block) {
        let (l_hot, l_cold) = (b.create_block(&[Type::Ptr, Type::Bool]), b.create_block(&[]));
        let (l_front, l_hash) = self.route(b, env, t, code);
        b.switch_to(l_front);
        let rec = b.block_params(l_front)[0];
        let c = b.load(Type::I64, rec, t.layout.sentinel_off() as i32);
        let held = b.icmp(IntCond::Ne, c, env.zero);
        let yes = b.bconst(true);
        b.cond_br(held, (l_hot, &[rec, yes]), (l_cold, &[]));
        b.switch_to(l_hash);
        if run && runs() {
            let (st, fr) = words(b, env, t);
            let last = b.load(Type::I64, st, fr + RW_CODE);
            let (same, l_hit) = (b.icmp(IntCond::Eq, code, last), b.create_block(&[]));
            b.cond_br(same, (l_hit, &[]), (l_cold, &[]));
            b.switch_to(l_hit);
            let (r, no) = (b.load(Type::Ptr, st, fr + RW_REC), b.bconst(false));
            b.br(l_hot, &[r, no]);
        } else {
            b.br(l_cold, &[]);
        }
        fires::fire("code.front.probe"); // fire signal (D3224 A4: a front probed with no key image)
        (l_hot, l_cold)
    }

    /// The front's record of in-front index `code` (null while it holds none), else the table's (hashed in that arm).
    pub(crate) fn find_at(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, code: Value) -> Value {
        let l_out = b.create_block(&[Type::Ptr]);
        let (l_front, l_hash) = self.route(b, env, t, code);
        b.switch_to(l_front);
        let rec = b.block_params(l_front)[0];
        let c = b.load(Type::I64, rec, t.layout.sentinel_off() as i32);
        let empty = b.icmp(IntCond::Eq, c, env.zero);
        let null = b.conv(ConvOp::IntToPtr, Type::Ptr, env.zero);
        let r = b.select(empty, null, rec);
        b.br(l_out, &[r]);
        b.switch_to(l_hash);
        let h = emit_key_hash(b, &img.words);
        let r = HASHED.find(b, env, t, img, h);
        b.br(l_out, &[r]);
        b.switch_to(l_out);
        b.block_params(l_out)[0]
    }
}

impl TableMech for Front {
    /// One code key, and no float slot: a front merges partial states (`Rec::fold`), and float partials reassociate,
    /// which can move or remove an overflow ERROR relative to PG's serial fold (V4R-2 keeps overflow exact).
    fn takes(&self, t: &TableDecl) -> bool {
        let float = (0..t.layout.nslots()).any(|j| matches!(t.layout.slot_kind(j), SlotKind::SumF64 | SlotKind::SumF32 | SlotKind::F64Accum));
        t.key.code.is_some() && !float
    }

    fn state_bytes(&self, t: &TableDecl) -> u32 {
        HASHED.state_bytes(t) + FR_WORDS + if runs() { core::mem::size_of::<RunWords>() as u32 } else { 0 }
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl) {
        fires::fire("code.front_part"); // shapecheck fire signal (REGISTRY NEW-lowerv4-34, RD-code.front_part)
        self.open_front(b, env, t);
    }

    /// A plain image (no code) upserts into the table, which may move the record the run cache holds: forgotten.
    fn upsert(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, h: Value) -> (Value, Value) {
        let Some(code) = img.code else {
            let r = HASHED.upsert(b, env, t, img, h);
            self.forget(b, env, t);
            return r;
        };
        self.upsert_at(b, env, t, img, code, true)
    }

    fn find(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, h: Value) -> Value {
        let Some(code) = img.code else { return HASHED.find(b, env, t, img, h) };
        self.find_at(b, env, t, img, code)
    }

    fn probe(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, codes: &ImageWords) -> Option<(gravity_ir::Block, gravity_ir::Block)> {
        Some(self.probe_at(b, env, t, codes.code?, true))
    }

    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl) {
        let x = env.x;
        self.flush(b, env, t, &x);
    }

    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, x: &Exits) {
        self.flush(b, env, t, x);
        HASHED.finish(b, env, t, x);
    }
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind};
    use gravity_abi::hash::Depth;

    use crate::table::bypass::HASHED;
    use crate::ops::agg::testkit::{fold, fold_records, want, Case};
    use super::FRONT;
    use crate::table::face::{Domain, KeyShape, TableDecl, TableMech};

    /// The front against the hashed table over part-local codes (each window its own code permutation), 10 000 values
    /// (codes past the front's capacity take the table), with and without NULL rows, a stop-free claim of 15 windows: the
    /// same records, and each key's rows and sum exact.
    #[test]
    fn the_front_folds_what_the_hashed_table_folds_over_part_local_codes() {
        let key: fn(i64) -> i64 = |i| i % 10_000;
        for null in [0, 97] {
            let n = 60_000;
            let front = fold(Case { n, win: 4096, key, table: &FRONT, run: None, runs: None, code: true, null, dense: None });
            let table = fold(Case { n, win: 4096, key, table: &HASHED, run: None, runs: None, code: true, null, dense: None });
            assert_eq!(front.0, want(n, key, null), "the front, null every {null}");
            assert_eq!(front.0, table.0, "the front against the table, null every {null}");
        }
    }

    /// The run cache: runs of two rows over 204,800 values (98 % of their codes past the 6,553-code front),
    /// with and without NULL rows, 100 windows: the same records as the hashed table, T1 bypassing, and at most one record
    /// a run (plus one a NULL row, which splits a run). Without the cache the front appended about one a row once T1
    /// bypassed: 278,529 records for 204,800 runs, as many as the table.
    #[test]
    fn a_run_past_the_front_folds_into_one_record() {
        let key: fn(i64) -> i64 = |i| (i / 2) % 400_000;
        let n = 100 * 4096;
        for null in [0, 97] {
            let (front, bypass, recs) = fold_records(Case { n, win: 4096, key, table: &FRONT, run: None, runs: None, code: true, null, dense: None });
            let (table, _, all) = fold_records(Case { n, win: 4096, key, table: &HASHED, run: None, runs: None, code: true, null, dense: None });
            assert_eq!(front, want(n, key, null), "the front, null every {null}");
            assert_eq!(front, table, "the front against the table, null every {null}");
            assert!(bypass, "T1 bypasses, null every {null}");
            let most = (n / 2 + if null == 0 { 0 } else { n / null + 1 }) as usize;
            assert!(recs <= most && recs < all, "{recs} records against at most {most} (the table's {all}), null every {null}");
        }
    }

    /// A record with a float slot never takes the front (main 09-24: float partials reassociate); one code key takes it.
    #[test]
    fn a_float_slot_declines_the_front() {
        let decl = |slots: &'static [SlotKind]| TableDecl {
            image_words: 2,
            record_bytes: 16,
            expect: None,
            depth: Depth::TOP,
            life: gravity_program::LifeId(0),
            layout: RecordLayout::with_sentinel(&[KeyKind::I64, KeyKind::I64], slots, false, None),
            state_off: 0,
            key: KeyShape { code: Some((crate::cell::LaneId::col(1), 0)), dense: None, domain: Domain::Unknown },
            facts: 0,
            copies: 0,
        };
        assert!(FRONT.takes(&decl(&[SlotKind::SumI64])));
        for f in [SlotKind::SumF64, SlotKind::SumF32, SlotKind::F64Accum] {
            let slots: &'static [SlotKind] = Box::leak(vec![SlotKind::SumI64, f].into_boxed_slice());
            assert!(!FRONT.takes(&decl(slots)), "{f:?}");
        }
    }
}
