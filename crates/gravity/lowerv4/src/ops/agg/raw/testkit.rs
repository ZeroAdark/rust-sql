//! The keyed tables' test kit (#[cfg(test)] only; declared by registry/m1.rs under `cfg(test)`). Budget 300 lines
//! (files.tsv). Owner M1-TABLES.
//!
//! A frame-driven keyed fold over a table mechanism: rows `0..n` in windows of `win` rows,
//! row `i` keyed by `key(i)`, its record counted (the record form's sentinel) and `i` summed into slot 0, through a
//! run carry when given; afterwards every record of T1 (its table and its partitions' blocks) is checked at its
//! KeyHash home pair or in its KeyHash partition, and each key's rows and sum are returned. In the code form the key is
//! a text value's stand-in: the image is two words (`v * FP_MUL + 1`, `v`), plus a flags word when rows are NULL, and
//! `ImageWords.code` is the value's code in the window's own dictionary (`(v + 5 w) % m` in window `w`: part-local
//! codes, all ones on a NULL row), as a pgrc code lane gives the code fronts. `fold_front`: the derived form through
//! the per-code front (keyed_codefront_percode) over the hashed table, the sum of `f(k) = k % 11 + 1` in place of `i`
//! (a function of the source code, as octet_length is), windows of `per_part` making a part (the site's PART_ID).
#![allow(unsafe_code)]
use gravity_abi::breaker::{HashAgg, KeyKind, Rec, RecordLayout, SlotKind, T1, T1_BYPASS};
use gravity_abi::ctx::{ctx_size, hdr, site_off};
use gravity_abi::hash::{Depth, KeyHash};
use gravity_abi::program::partial;
use gravity_abi::site::site;
use gravity_abi::{status, Ctx, Partials, ProgramBlock};
use gravity_expr::emit::hash::{emit_key_hash, ImageValues};
use gravity_interp::Interp;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};

use crate::face::end::EndMech;
use crate::table::face::{Domain, KeyShape, RunCarry, RunFold, TableDecl, TableMech};
use crate::table::keyimg::{FrontIndex, ImageWords};
use crate::face::source::SourceMech;
use crate::frame::env::{self, Env};
use crate::frame::frame::{Mechs, Win};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use gravity_kernels::dict::strategies::percode::Fold as PcFold;

use crate::dict::strategies::codefront_percode::Percode;

const I64: Type = Type::I64;
const SHARE: usize = 16 << 20;
const ARENA: usize = 1 << 20;
pub const LAYOUT: RecordLayout = RecordLayout::with_sentinel(&[KeyKind::I64], &[SlotKind::SumI64], false, None);
const CODE: RecordLayout = RecordLayout::with_sentinel(&[KeyKind::I64, KeyKind::I64], &[SlotKind::SumI64], false, None);
const CODE_NULL: RecordLayout = RecordLayout::with_sentinel(&[KeyKind::I64, KeyKind::I64, KeyKind::I64], &[SlotKind::SumI64], false, None);
const PAIR: RecordLayout = RecordLayout::with_sentinel(&[KeyKind::I64; 4], &[SlotKind::SumI64], false, None);
const PAIR_NULL: RecordLayout = RecordLayout::with_sentinel(&[KeyKind::I64; 5], &[SlotKind::SumI64], false, None);
const FP_MUL: i64 = 0x2545_F491_4F6C_DD1Du64 as i64;

/// `Case::runs` for windows the source calls constant-key (`WinConds::const_key`, V4R-19 A), with runs word 1.
pub const CONST_KEY: i64 = i64::MIN;

/// One fold: `n` rows in windows of `win`, keyed by `key`, into `table`, through `run` with the windows' runs word.
pub struct Case {
    pub n: i64,
    pub win: i64,
    pub key: fn(i64) -> i64,
    pub table: &'static dyn TableMech,
    pub run: Option<&'static dyn RunCarry>,
    pub runs: Option<i64>,
    /// The code form (0: an int key); with it, every `null`-th row's key is NULL (0: none).
    pub code: bool,
    pub null: i64,
    /// code.front_dense's forms (with `code`): Some(false) a pair of code keys `(k % 7, k / 7)` (site lanes 0 and 1, their
    /// dictionaries 7 and `ceil(m / 7)` codes), Some(true) the value `k / 3` derived from `k`'s code
    /// (`ImageWords.index` = the source code; three codes share a value).
    pub dense: Option<bool>,
}

impl Case {
    fn layout(&self) -> RecordLayout {
        let (n, null) = (if self.dense == Some(false) { 4 } else { 2 }, self.null != 0);
        match (self.code, n + null as usize) {
            (false, _) => LAYOUT,
            (true, 2) => CODE,
            (true, 3) => CODE_NULL,
            (true, 4) => PAIR,
            _ => PAIR_NULL,
        }
    }
}

struct Src(i64, i64, Option<i64>, Option<i64>);

impl SourceMech for Src {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("cursor", Base::Ctx, (site_off(0) + site::CURSOR) as i32, Type::I32, 0));
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Value {
        b.iconst(I64, 0)
    }
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let cursor = b.load_zext(Type::I32, env.ctx, (site_off(0) + site::CURSOR) as i32);
        let (n, w) = (b.iconst(I64, self.0), b.iconst(I64, self.1));
        let past = b.icmp(IntCond::Sge, cursor, n);
        let (l_end, l_win) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(past, (l_end, &[]), (l_win, &[]));
        b.switch_to(l_end);
        b.br(l_eof, &[env.zero]);
        b.switch_to(l_win);
        let next = b.int_bin(IntBinOp::Add, cursor, w);
        let over = b.icmp(IntCond::Sgt, next, n);
        let end = b.select(over, n, next);
        if let Some(pp) = self.3 {
            let per = b.iconst(I64, self.1 * pp);
            let part = b.int_bin(IntBinOp::Udiv, cursor, per);
            let part = b.conv(gravity_ir::ConvOp::Trunc, Type::I32, part);
            b.store(env.ctx, (site_off(0) + site::PART_ID) as i32, part);
        }
        let mut facts = crate::face::source::WinConds::default();
        facts.runs = self.2.map(|r| b.iconst(I64, if r == CONST_KEY { 1 } else { r }));
        facts.const_key = (self.2 == Some(CONST_KEY)).then(|| (b.bconst(true), b.iconst(I64, 0)));
        Win { cursor, end, facts }
    }
    fn row<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        RowVals { row: crate::cell::Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry, next, at: j }
    }
}

struct Fold(fn(i64) -> i64, TableDecl, &'static dyn TableMech, Option<&'static dyn RunCarry>, i64, bool, i64, Option<bool>, Option<Percode>);

impl Fold {
    fn image(&self, b: &mut FunctionBuilder<'_>, at: Value) -> ImageWords {
        let (k, m) = key_ir(b, self.0, at);
        if !self.5 {
            return ImageWords::plain(ImageValues { words: vec![k] }, true);
        }
        let (mul, one, zero, ones) = (b.const_raw(I64, FP_MUL as u64), b.iconst(I64, 1), b.iconst(I64, 0), b.iconst(I64, -1));
        let (win, five) = (b.iconst(I64, self.4), b.iconst(I64, 5));
        let w = b.int_bin(IntBinOp::Udiv, at, win);
        let w5 = b.int_bin(IntBinOp::Mul, w, five);
        // value `v`'s code in the window's own dictionary of `n` values (part-local codes)
        let code_of = |b: &mut FunctionBuilder<'_>, v: Value, n: i64| {
            let (c, nv) = (b.int_bin(IntBinOp::Add, v, w5), b.iconst(I64, n));
            b.int_bin(IntBinOp::Urem, c, nv)
        };
        let (seven, three) = (b.iconst(I64, 7), b.iconst(I64, 3));
        let (vals, codes) = match self.7 {
            None => (vec![k], vec![code_of(b, k, m)]),
            Some(false) => {
                let (k0, k1) = (b.int_bin(IntBinOp::Urem, k, seven), b.int_bin(IntBinOp::Udiv, k, seven));
                (vec![k0, k1], vec![code_of(b, k0, 7), code_of(b, k1, (m + 6) / 7)])
            }
            Some(true) => (vec![b.int_bin(IntBinOp::Udiv, k, three)], vec![code_of(b, k, m)]),
        };
        let mut words: Vec<Value> = Vec::new();
        for v in vals {
            let fp = b.int_bin(IntBinOp::Mul, v, mul);
            words.extend([b.int_bin(IntBinOp::Add, fp, one), v]);
        }
        let mut codes = codes;
        if self.6 != 0 {
            let nv = b.iconst(I64, self.6);
            let r = b.int_bin(IntBinOp::Urem, at, nv);
            let null = b.icmp(IntCond::Eq, r, zero);
            words = words.into_iter().map(|w| b.select(null, zero, w)).collect();
            words.push(b.select(null, one, zero));
            codes = codes.into_iter().map(|c| b.select(null, ones, c)).collect();
        }
        let (code, index) = match self.7 {
            None => (Some(codes[0]), None),
            Some(false) => (None, Some(FrontIndex::Codes(vec![(codes[0], 0), (codes[1], 1)]))),
            Some(true) => (None, Some(FrontIndex::Source(codes[0]))),
        };
        ImageWords { words: ImageValues { words }, exact: true, code, index, widths: Vec::new() }
    }
    fn add(&self, b: &mut FunctionBuilder<'_>, rec: Value, rows: Value, sum: Value) {
        for (off, v) in [(self.1.layout.sentinel_off() as i32, rows), (self.1.layout.slot_off(0) as i32, sum)] {
            let o = b.load(I64, rec, off);
            let o = b.int_bin(IntBinOp::Add, o, v);
            b.store(rec, off, o);
        }
    }
}

impl RunFold for Fold {
    fn init<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> mcx::PgVec<'e, Value> {
        mcx::pg_vec![in env.mcx; b.iconst(I64, 0), b.iconst(I64, 0)]
    }
    fn step<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, accs: &[Value]) -> mcx::PgVec<'e, Value> {
        mcx::pg_vec![in env.mcx; b.int_bin(IntBinOp::Add, accs[0], env.one), b.int_bin(IntBinOp::Add, accs[1], row.at)]
    }
    fn close(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, img: &ImageWords, h: Value, accs: &[Value]) {
        let (rec, _) = self.2.upsert(b, env, &self.1, img, h);
        self.add(b, rec, accs[0], accs[1]);
    }
    fn row_into(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, img: &ImageWords, h: Value) {
        let (rec, _) = self.2.upsert(b, env, &self.1, img, h);
        self.add(b, rec, env.one, row.at);
    }
}

impl EndMech for Fold {
    fn state_bytes(&self) -> u32 {
        self.2.state_bytes(&self.1) + self.8.map_or(0, |_| crate::dict::strategies::codefront_percode::WORDS)
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.2.open(b, env, &self.1);
        self.8.inspect(|p| p.open(b, env, &self.1));
    }
    fn carried<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> mcx::PgVec<'e, Value> {
        self.3.map_or(mcx::PgVec::new_in(env.mcx), |r| r.carried(b, env, &self.1, self))
    }
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        let img = self.image(b, row.at);
        match self.3 {
            Some(r) => {
                let win = env.win.expect("a run carry inside a window");
                let accs = r.row(b, env, &self.1, &win, row, &img, &row.carry.accs, self);
                Carry { out: row.carry.out, accs }
            }
            None if self.8.is_some() => {
                let (k, _) = key_ir(b, self.0, row.at);
                let (eleven, one) = (b.iconst(I64, 11), b.iconst(I64, 1));
                let f = b.int_bin(IntBinOp::Urem, k, eleven);
                let f = b.int_bin(IntBinOp::Add, f, one);
                let l_done = self.8.map(|p| p.row(b, env, &self.1, &img, |_, _| Some(f))).expect("a front");
                let h = emit_key_hash(b, &img.words);
                let (rec, _) = self.2.upsert(b, env, &self.1, &img, h);
                self.add(b, rec, env.one, f);
                b.br(l_done, &[]);
                b.switch_to(l_done);
                Carry { out: row.carry.out, accs: mcx::PgVec::new_in(env.mcx) }
            }
            None => {
                let h = emit_key_hash(b, &img.words);
                self.row_into(b, env, row, &img, h);
                Carry { out: row.carry.out, accs: mcx::PgVec::new_in(env.mcx) }
            }
        }
    }
    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, c: &Carry) {
        if let Some(r) = self.3 {
            r.window_end(b, env, &self.1, &c.accs, self);
        }
        self.2.window_end(b, env, &self.1);
        self.8.inspect(|p| p.window_end(b, env, &self.1));
    }
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        self.8.inspect(|p| p.finish(b, env, &self.1, x));
        self.2.finish(b, env, &self.1, x)
    }
}

gravity_abi::host! {
    /// The claim's arena never grows (a draw past it is refused).
    unsafe fn no_grow(_ctx: *mut gravity_abi::Ctx, _bytes: u64) -> i64 {
        0
    }
}

/// `key(i)` in IR and its period: the test keys are `(i / d) % m`, recovered from the function's values.
/// A periodic key's step `d` and period `m`: `key(i) = f(i / d % m)`.
fn period(key: fn(i64) -> i64) -> (i64, i64) {
    let d = (1..1 << 20).find(|&d| key(d) != key(0)).expect("a key that changes");
    (d, (1..1 << 24).find(|&m| key(m * d) == key(0)).expect("a periodic key"))
}

fn key_ir(b: &mut FunctionBuilder<'_>, key: fn(i64) -> i64, at: Value) -> (Value, i64) {
    let (d, m) = period(key);
    let (dv, mv) = (b.iconst(I64, d), b.iconst(I64, m));
    let q = b.int_bin(IntBinOp::Udiv, at, dv);
    (b.int_bin(IntBinOp::Urem, q, mv), m)
}

/// The fold's T1 afterwards: each key's `(rows, sum)` and whether the table bypassed.
pub fn fold(c: Case) -> (std::collections::BTreeMap<i64, (i64, i64)>, bool) {
    let r = fold_in(c, None, None, ARENA);
    (r.0, r.1)
}

/// `fold`, and the records T1 holds (its table's and its partitions' blocks').
pub fn fold_records(c: Case) -> (std::collections::BTreeMap<i64, (i64, i64)>, bool, usize) {
    let r = fold_in(c, None, None, ARENA);
    (r.0, r.1, r.3)
}

/// `fold` of the derived form (`c.dense` Some(true)) through a per-code front of `cap` codes over the hashed table, a part
/// every `per_part` windows, the claim's arena `arena` bytes: each key's `(rows, sum of f)` and whether the front's region
/// came from the arena (`pc_bytes` of it drawn; a refused one leaves every row to the per-row path).
pub fn fold_front(c: Case, cap: u32, per_part: i64, arena: usize) -> (std::collections::BTreeMap<i64, (i64, i64)>, bool) {
    let (front, rec) = (Percode::of(PcFold::Sum64.at(0) | PcFold::Count.at(1), c.null != 0, cap), c.layout().size() as u64);
    let (keys, _, drawn, _) = fold_in(Case { table: &crate::table::bypass::HASHED, ..c }, Some(front), Some(per_part), arena);
    (keys, drawn as u64 >= gravity_kernels::dict::strategies::percode::pc_bytes(rec))
}

fn fold_in(c: Case, front: Option<Percode>, per_part: Option<i64>, arena_bytes: usize) -> (std::collections::BTreeMap<i64, (i64, i64)>, bool, usize, usize) {
    let rig = gravity_driver::mem::root::Root::new("GravityPlanRig");
    let l = c.layout();
    let lane = (crate::cell::LaneId::col(1), 0);
    let code = (c.code && c.dense.is_none()).then_some(lane);
    let dense = c.dense.map(|derived| crate::table::face::DenseFront { n: if derived { 1 } else { 2 }, derived, groups: 0 });
    let t = TableDecl { image_words: l.nkeys() as u16, record_bytes: 16, expect: None, depth: Depth::TOP, life: gravity_program::LifeId(0), layout: l, state_off: 0, key: KeyShape { code, dense, domain: Domain::Unknown }, facts: 0, copies: 0 };
    let desc = gravity_program::Pipeline::eligible(0, gravity_program::End::Sink, 0);
    let end = mcx::pg_box_dyn!(rig.mcx(), Fold(c.key, t, c.table, c.run, c.win, c.code, c.null, c.dense, front) => dyn EndMech);
    let mut m = Mechs { k: 0, source: mcx::pg_box_dyn!(rig.mcx(), Src(c.n, c.win, c.runs, per_part) => dyn SourceMech), steps: mcx::PgVec::new_in(rig.mcx()), end, site: Some(0), desc, words: None, state_word: None };
    let mut pb = ProgramBuilder::new();
    let core = env::declare(&mut pb);
    let (f, _) = crate::frame::frame::pipeline_fn(&mut pb, core, crate::hir::Ir::none(), &[], "p0", 1, &mut m, rig.mcx());
    let p = pb.finish();
    gravity_ir::verify::verify(&p).expect("the table's IR verifies");
    let mut own = vec![0u64; SHARE / 8];
    unsafe { HashAgg::arm(own.as_mut_ptr() as *mut u8, SHARE as u32) };
    let mut partials = Box::new(Partials::two(own.as_mut_ptr() as *mut u8, [core::ptr::null_mut(); 2]));
    let mut program = Box::new(ProgramBlock::NONE);
    program.partial = &mut *partials;
    program.arena_grow = Some(no_grow);
    let (mut ctx, mut out) = (vec![0u64; ctx_size(1) / 8], [0u64; 32]);
    ctx[hdr::OUT / 8] = out.as_mut_ptr() as u64;
    let cp = ctx.as_mut_ptr() as *mut Ctx;
    // the claim's arena window (the code front's records come from it: `agg_front_open`)
    let mut arena = vec![0u64; arena_bytes / 8];
    // the pair's site lanes: code.front_dense reads their dictionary sizes
    let m = period(c.key).1 as u32;
    let lanes = [gravity_abi::Lane { kind: gravity_abi::lane::KIND_CODES, ncodes_or_scale: 7, ..gravity_abi::Lane::NONE }, gravity_abi::Lane { kind: gravity_abi::lane::KIND_CODES, ncodes_or_scale: m.div_ceil(7), ..gravity_abi::Lane::NONE }];
    unsafe {
        *((cp as *mut u8).add(site_off(0) + site::LANES) as *mut *const gravity_abi::Lane) = lanes.as_ptr();
        (*cp).program = &mut *program;
        (*cp).sites = (cp as *mut u8).add(site_off(0)) as *mut gravity_abi::Site;
        (*cp).arena_cur = arena.as_mut_ptr() as *mut u8;
        (*cp).arena_end = (arena.as_mut_ptr() as *mut u8).add(arena_bytes);
    }
    assert_eq!(Interp::new(&p).run(f, &[cp as u64]).expect("runs")[0] as i64, status::DONE);
    let t1 = unsafe { T1::view(partial(cp)) };
    let (mut keys, mut records) = (std::collections::BTreeMap::new(), 0);
    let mut take = |r: Rec<'_>| {
        records += 1;
        let k = match (c.code, l.nkeys()) {
            (false, _) => r.key(0) as i64,
            (true, 3) if r.key(2) != 0 => -1,
            (true, 5) if r.key(4) != 0 => -1,
            (true, 4 | 5) => (r.key(1) + 7 * r.key(3)) as i64,
            _ => r.key(1) as i64,
        };
        let e = keys.entry(k).or_insert((0, 0));
        e.0 += r.count(1) - 1;
        e.1 += r.i64(0);
    };
    for i in 0..t1.cap as usize {
        let r = t1.record(i);
        if r.used() {
            let home = KeyHash::of(r.keys()).home(t1.cap.trailing_zeros(), Depth::TOP);
            assert!(i == home || i == (home + 1) % t1.cap as usize, "key {} at slot {i}, home {home}", r.key(0));
            take(r);
        }
    }
    for part in 0..t1.parts() as u32 {
        for ch in t1.chain(part) {
            for j in 0..ch.recs {
                let r = unsafe { Rec::at((ch.ptr as *mut u8).add(j * l.size() as usize), &l) };
                assert_eq!(KeyHash::of(r.keys()).part(t1.pbits as u32), part, "key {} in its partition", r.key(0));
                take(r);
            }
        }
    }
    let drawn = unsafe { (*cp).arena_cur as usize - arena.as_ptr() as usize };
    (keys, t1.flags & T1_BYPASS != 0, drawn, records)
}

/// Each key's `(rows, sum)` over rows `0..n`; with `null`, every `null`-th row under key -1 (NULL).
pub fn want(n: i64, key: fn(i64) -> i64, null: i64) -> std::collections::BTreeMap<i64, (i64, i64)> {
    let mut w = std::collections::BTreeMap::new();
    for i in 0..n {
        let k = if null != 0 && i % null == 0 { -1 } else { key(i) };
        let e = w.entry(k).or_insert((0, 0));
        e.0 += 1;
        e.1 += i;
    }
    w
}
