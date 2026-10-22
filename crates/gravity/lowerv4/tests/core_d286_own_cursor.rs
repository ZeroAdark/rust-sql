//! D286 (M1-TABLES): a pipeline whose source has no site and writes back its own cursor (a breaker's emit: keyed/emit_group.rs,
//! runs/topk_groups.rs, keyed/distinct_plain.rs, source/values.rs, source/result.rs) runs every window, not only the first.
//! frame/frame.rs's `pipeline_fn` gave that source `l_done` after a window, so emit_group's 4,096-record windows returned the
//! first 4,096 groups of any larger GROUP BY. Each own-cursor source's window contract is modelled here under the
//! interpreter: windows of a fixed width over more records than one (emit_group, topk_groups), one record (distinct_plain),
//! the whole range at once (values), and a `once` source (result), which still finishes after its one window.
use std::sync::mpsc;
use std::time::Duration;

use gravity_abi::ctx::{ctx_size, hdr};
use gravity_abi::status::DONE;
use gravity_interp::Interp;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
use gravity_lowerv4::face::{end::EndMech, source::SourceMech};
use gravity_lowerv4::frame::frame::{self, Mechs, Win};
use gravity_lowerv4::frame::open::{Base, OpenWord, OpenWords};
use gravity_lowerv4::frame::row::{Carry, RowVals};
use gravity_lowerv4::frame::{cancel, env, env::Env, stop::Exits};
use gravity_lowerv4::hir::Ir;

const I64: Type = Type::I64;
/// The source's own cursor and the sum its write-back accumulates: words past the one-site context.
const CUR: usize = ctx_size(1) / 8;
const SUM: usize = CUR + 1;
/// The status a finished source's eof returns (never the `done` host's).
const EOF: i64 = -5;

/// `total` records `j`, each worth `j + 1`, in windows of `window` from the source's own cursor; `once`: result's form.
struct Own {
    total: i64,
    window: i64,
    once: bool,
}

impl SourceMech for Own {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("own cursor", Base::Ctx, (CUR * 8) as i32, I64, 0));
        w.declare(OpenWord::open("own sum", Base::Ctx, (SUM * 8) as i32, I64, 0));
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Value {
        b.iconst(I64, 0)
    }
    fn once(&self) -> bool {
        self.once
    }
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let cursor = b.load(I64, env.ctx, (CUR * 8) as i32);
        let n = b.iconst(I64, self.total);
        let past = b.icmp(IntCond::Sge, cursor, n);
        let (l_end, l_win) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(past, (l_end, &[]), (l_win, &[]));
        b.switch_to(l_end);
        let st = b.iconst(I64, EOF);
        b.br(l_eof, &[st]);
        b.switch_to(l_win);
        let w = b.iconst(I64, self.window);
        let end = b.int_bin(IntBinOp::Add, cursor, w);
        let over = b.icmp(IntCond::Sgt, end, n);
        let end = b.select(over, n, end);
        Win { cursor, end, facts: Default::default() }
    }
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let v = b.int_bin(IntBinOp::Add, j, env.one);
        RowVals { row: gravity_lowerv4::cell::Row::new_in(env.mcx), vals: mcx::pg_vec![in env.mcx; (v, None)], cols: mcx::PgVec::new_in(env.mcx), carry, next, at: j }
    }
    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, out: Value) {
        b.store(env.ctx, (CUR * 8) as i32, cur);
        let s = b.load(I64, env.ctx, (SUM * 8) as i32);
        let s = b.int_bin(IntBinOp::Add, s, out);
        b.store(env.ctx, (SUM * 8) as i32, s);
    }
}

/// Sums the rows' values.
struct Sum;

impl EndMech for Sum {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        Carry::new(b.int_bin(IntBinOp::Add, row.carry.out, row.vals[0].0), env.mcx)
    }
}

/// The site-less pipeline `Own → Sum` over a fresh context (an empty output block, so the `done` host's flush has nothing
/// to push), on its own thread under a 20 s watchdog (a source that never reaches eof fails, never hangs): its status, the
/// cursor and the sum.
fn run(total: i64, window: i64, once: bool) -> (i64, i64, i64) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let mut desc = gravity_program::Pipeline::eligible(0, gravity_program::End::Sink, 0);
        desc.driving_site = -1;
        let mut m = Mechs { k: 0, source: mcx::pg_box_dyn!(rig.mcx(), Own { total, window, once } => dyn SourceMech), steps: mcx::PgVec::new_in(rig.mcx()), end: mcx::pg_box_dyn!(rig.mcx(), Sum => dyn EndMech), site: None, desc, words: None, state_word: None };
        let mut pb = ProgramBuilder::new();
        let core = env::declare(&mut pb);
        let (f, bounded) = frame::pipeline_fn(&mut pb, core, Ir::none(), &[], "p0", 1, &mut m, rig.mcx());
        let p = pb.finish();
        gravity_ir::verify::verify(&p).expect("the frame's IR verifies");
        assert_eq!(cancel::verify(p.func(f), &bounded, rig.mcx()), Ok(()));
        let mut out = vec![0u64; 7];
        let mut ctx = vec![0u64; SUM + 1];
        ctx[hdr::OUT / 8] = out.as_mut_ptr() as u64;
        let st = Interp::new(&p).run(f, &[ctx.as_mut_ptr() as u64]).expect("runs")[0] as i64;
        let _ = tx.send((st, ctx[CUR] as i64, ctx[SUM] as i64));
    });
    rx.recv_timeout(Duration::from_secs(20)).expect("the pipeline ends within 20 s (no window loop past eof)")
}

/// `1 + 2 + … + n`.
fn tri(n: i64) -> i64 {
    n * (n + 1) / 2
}

#[test]
fn a_source_writing_its_own_cursor_runs_every_window_not_only_the_first() {
    // emit_group / topk_groups: 8,000 groups in 4,096-record windows (two windows, the second a tail), every group back once
    assert_eq!(run(8000, 4096, false), (EOF, 8000, tri(8000)));
    // three whole windows and a tail
    assert_eq!(run(3 * 70 + 6, 70, false), (EOF, 216, tri(216)));
    // exactly one window's worth: one window, then eof
    assert_eq!(run(4096, 4096, false), (EOF, 4096, tri(4096)));
    // distinct_plain: its one record [0, 1), then eof
    assert_eq!(run(1, 1, false), (EOF, 1, 1));
    // values: the whole list as one window [pos, len), then eof
    assert_eq!(run(100, 100, false), (EOF, 100, tri(100)));
    // no records: eof at the first window
    assert_eq!(run(0, 4096, false), (EOF, 0, 0));
}

#[test]
fn a_once_source_still_finishes_after_its_one_window() {
    // result: one window, then done (the `done` host's status), never a second window
    assert_eq!(run(1, 1, true), (DONE, 1, 1));
}
