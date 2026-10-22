//! D370's Defect-Test (M1-TABLES, main 09-24): a count never reaches `agg_open`'s flag words. keyed/dense_small.rs opened
//! the hashed table at a bound domain's span, `hi - lo + 1` as an unsigned difference: a high-cardinality int8 key's span
//! (2^64 - 1.1e13) set AGG_GROUPS_OBSERVE and AGG_GROUPS_BYPASS (1 << 62, 1 << 61) and held its T1 (T1_HOLD). The open
//! (keyed/table_size.rs `open_with`) now takes a count within the table's bound (TableSize) and below the flag words. The
//! resolver's table for one int8 key over a bound domain (the small-domain table, hashed past its slots) folds rows keyed
//! `i % 23` under Interp, the domain and TableSize in its Facts words; T1's posture and every key's rows are checked. The
//! rows against PostgreSQL: e2e/gravity/v4/m1_d370_span.sql.
use gravity_abi::breaker::{HashAgg, KeyKind, Rec, RecordLayout, SlotKind, T1, T1_BYPASS, T1_EST, T1_HOLD};
use gravity_abi::ctx::{ctx_size, hdr, site_off};
use gravity_abi::hash::Depth;
use gravity_abi::program::partial;
use gravity_abi::site::site;
use gravity_abi::{status, Ctx, Partials, ProgramBlock};
use gravity_expr::emit::hash::{emit_key_hash, ImageValues};
use gravity_interp::Interp;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
use gravity_kernels::host_abi::{AGG_GROUPS_BYPASS, AGG_GROUPS_OBSERVE};
use gravity_lowerv4::table::face as keyed;
use gravity_lowerv4::table::face::{Domain, KeyShape, TableDecl, TableMech};
use gravity_lowerv4::face::end::EndMech;
use gravity_lowerv4::table::keyimg::ImageWords;
use gravity_lowerv4::face::source::SourceMech;
use gravity_lowerv4::frame::frame::{self, Mechs, Win};
use gravity_lowerv4::frame::open::{Base, OpenWord, OpenWords};
use gravity_lowerv4::frame::row::{Carry, RowVals};
use gravity_lowerv4::frame::{env, env::Env, stop::Exits};
use gravity_lowerv4::hir::Ir;

const I64: Type = Type::I64;
const SHARE: usize = 16 << 20;
const LAYOUT: RecordLayout = RecordLayout::with_sentinel(&[KeyKind::I64], &[SlotKind::SumI64], false, None);
const N: i64 = 10_000;
const G: i64 = 23;

struct Node;

impl gravity_lowerv4::admit::refusal::Site for Node {
    fn node(&self) -> i32 {
        1
    }
}

/// Rows `[cursor, N)` in one window.
struct Src;

impl SourceMech for Src {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("cursor", Base::Ctx, (site_off(0) + site::CURSOR) as i32, Type::I32, 0));
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Value {
        b.iconst(I64, 0)
    }
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let cursor = b.load_zext(Type::I32, env.ctx, (site_off(0) + site::CURSOR) as i32);
        let end = b.iconst(I64, N);
        let past = b.icmp(IntCond::Sge, cursor, end);
        let (l_end, l_win) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(past, (l_end, &[]), (l_win, &[]));
        b.switch_to(l_end);
        b.br(l_eof, &[env.zero]);
        b.switch_to(l_win);
        Win { cursor, end, facts: Default::default() }
    }
    fn row<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        RowVals { row: gravity_lowerv4::cell::Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry, next, at: j }
    }
}

/// Row `i` keyed by `i % G`: its record counted in the sentinel and `i` summed into slot 0; no finish (the test reads T1
/// and its partitions as the claim left them).
struct Fold(TableDecl, &'static dyn TableMech);

impl EndMech for Fold {
    fn state_bytes(&self) -> u32 {
        self.1.state_bytes(&self.0)
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.1.open(b, env, &self.0)
    }
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        let g = b.iconst(I64, G);
        let k = b.int_bin(IntBinOp::Urem, row.at, g);
        let img = ImageWords::plain(ImageValues { words: vec![k] }, true);
        let h = emit_key_hash(b, &img.words);
        let (rec, _fresh) = self.1.upsert(b, env, &self.0, &img, h);
        for (off, add) in [(LAYOUT.sentinel_off() as i32, env.one), (LAYOUT.slot_off(0) as i32, row.at)] {
            let v = b.load(I64, rec, off);
            let v = b.int_bin(IntBinOp::Add, v, add);
            b.store(rec, off, v);
        }
        Carry { out: row.carry.out, accs: mcx::PgVec::new_in(env.mcx) }
    }
}

/// The fold over the domain `[lo, hi]` with TableSize `size`: T1's flags, and every key's (rows, sum) over T1's records
/// and its partitions' buffers.
fn fold(lo: i64, hi: i64, size: u64) -> (u8, Vec<(i64, i64)>) {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    let key = KeyShape { code: None, dense: None, domain: Domain::Bound };
    let t = TableDecl { image_words: 1, record_bytes: 16, expect: None, depth: Depth::TOP, life: gravity_program::LifeId(0), layout: LAYOUT, state_off: 0, key, facts: 1, copies: 0 };
    let desc = gravity_program::Pipeline::eligible(0, gravity_program::End::Sink, 0);
    let mut m = Mechs { k: 0, source: mcx::pg_box_dyn!(rig.mcx(), Src => dyn SourceMech), steps: mcx::PgVec::new_in(rig.mcx()), end: mcx::pg_box_dyn!(rig.mcx(), Fold(t, keyed::table(&Node, &t)) => dyn EndMech), site: Some(0), desc, words: None, state_word: None };
    let mut pb = ProgramBuilder::new();
    let core = env::declare(&mut pb);
    let (f, _) = frame::pipeline_fn(&mut pb, core, Ir::none(), &[], "p0", 1, &mut m, rig.mcx());
    let p = pb.finish();
    gravity_ir::verify::verify(&p).expect("the table's IR verifies");
    let mut own = vec![0u64; SHARE / 8];
    unsafe { HashAgg::arm(own.as_mut_ptr() as *mut u8, SHARE as u32) };
    let mut partials = Box::new(Partials::two(own.as_mut_ptr() as *mut u8, [core::ptr::null_mut(); 2]));
    let mut program = Box::new(ProgramBlock::NONE);
    program.partial = &mut *partials;
    let (mut ctx, mut out, params) = (vec![0u64; ctx_size(1) / 8], [0u64; 32], [0, lo as u64, hi as u64, size]);
    ctx[hdr::OUT / 8] = out.as_mut_ptr() as u64;
    ctx[hdr::PARAMS / 8] = params.as_ptr() as u64;
    let c = ctx.as_mut_ptr() as *mut Ctx;
    unsafe { (*c).program = &mut *program };
    assert_eq!(Interp::new(&p).run(f, &[c as u64]).expect("runs")[0] as i64, status::DONE);
    let t1 = unsafe { T1::view(partial(c)) };
    let mut got = vec![(0i64, 0i64); G as usize];
    let mut add = |r: &Rec| {
        let e = &mut got[r.key(0) as usize];
        *e = (e.0 + r.count(1) - 1, e.1 + r.i64(0));
    };
    (0..t1.cap as usize).map(|i| t1.record(i)).filter(|r| r.used()).for_each(|r| add(&r));
    for part in 0..t1.parts() as u32 {
        for ch in t1.chain(part) {
            (0..ch.recs).for_each(|j| add(&unsafe { Rec::at((ch.ptr as *mut u8).add(j * LAYOUT.size() as usize), &LAYOUT) }));
        }
    }
    (t1.flags, got)
}

#[test]
fn a_wide_bound_domain_opens_at_its_bound_below_the_flag_words() {
    let want: Vec<(i64, i64)> = (0..G).map(|k| (0..N).filter(|i| i % G == k).fold((0, 0), |(n, s), i| (n + 1, s + i))).collect();
    let q4 = (i64::MIN + 5_415_166_039_285, i64::MAX - 5_415_166_039_285);
    // the OBSERVE word alone (a span past 2^62), q4's span (past 2^63, both words) at q4's 100 M rows (a bound past the
    // slots asks the estimate's bypass, T1_EST), and a TableSize carrying both words
    let cases = [((0, (1 << 62) + 22), 10_000, 0), (q4, 99_997_497, T1_EST), (q4, AGG_GROUPS_OBSERVE | AGG_GROUPS_BYPASS | 23, T1_EST)];
    for ((lo, hi), size, est) in cases {
        let (flags, got) = fold(lo, hi, size);
        assert_eq!(flags & (T1_HOLD | T1_BYPASS | T1_EST), est, "domain [{lo}, {hi}], TableSize {size:#x}: the open's posture");
        assert!(got == want, "domain [{lo}, {hi}], TableSize {size:#x}: per-key rows and sums");
    }
}
