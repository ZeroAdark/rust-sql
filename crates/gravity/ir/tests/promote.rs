//! The register-state pass (`promote`) and its verifier on hand-written
//! programs (`docs/design/gravity-simple/register-state.md` §3).

use gravity_ir::promote::{promote, Cell, Root, TooDeep};

/// The walk depth the lowerer passes (gravity_abi `limits::PROMOTE_MAX_READ_DEPTH`).
const DEPTH: u32 = 2048;
use gravity_ir::text::{parse_program, print_program};
use gravity_ir::verify::{verify, verify_carried};
use gravity_ir::*;

fn load(src: &str) -> Program {
    parse_program(src, &|_| Some(0)).unwrap_or_else(|e| panic!("{e}\n{src}"))
}

/// Each host's state mask, by host index (what `promote` and `verify_carried` read).
fn masks(p: &Program) -> Vec<StateMask> {
    p.host_fns.iter().map(|h| h.flags.state).collect()
}

/// The ctx parameter of function 0.
fn ctx(p: &Program) -> Value {
    p.funcs[0].blocks[p.funcs[0].entry.index()].params[0]
}

fn cell(p: &Program, offset: i32, ty: Type, class: StateMask) -> Cell {
    Cell { root: Root::Value(ctx(p)), offset, ty, class }
}

/// Run the pass on function 0 with `cells`; verify structurally and
/// against the pre-pass function; return the program and the report.
fn run(src: &str, cells: &[Cell]) -> (Program, Program, gravity_ir::promote::Report) {
    let mut p = load(src);
    verify(&p).unwrap();
    let f0 = p.dup_func(Func(0));
    let hosts = masks(&p);
    let report = promote(&mut p.funcs[0], cells, &hosts, DEPTH).expect("promote");
    let text = print_program(&p);
    verify(&p).unwrap_or_else(|e| panic!("{e:?}\n{text}"));
    let m = p.mint();
    verify_carried(&m, &f0, &p.funcs[0], cells, &report, &hosts).unwrap_or_else(|e| panic!("{e}\n{text}"));
    let mut p0 = p.clone();
    p0.funcs[0] = f0.clone_in(&p0.mint());
    (p0, p, report)
}

fn count_ops(f: &Function, b: usize, pred: impl Fn(&Op) -> bool) -> usize {
    f.blocks[b].insts.iter().filter(|i| pred(&i.op)).count()
}

fn is_cell_access(op: &Op, offset: i32) -> bool {
    matches!(op, Op::Load { offset: o, .. } | Op::LoadZext { offset: o, .. } | Op::Store { offset: o, .. } if *o == offset)
}

const COUNTER: &str = r#"
(program
  (host sink (ptr) void state=1)
  (fn f ((ctx ptr)) (i64)
    (block entry ((ctx ptr))
      (n (load i64 ctx 8))
      (zero (const i64 0))
      (br loop zero))
    (block loop ((i i64))
      (c (load i64 ctx 0))
      (one (const i64 1))
      (c1 (iadd c one))
      ((store ctx 0 c1))
      (i1 (iadd i one))
      (done (icmp sge i1 n))
      (condbr done (exit) (loop i1)))
    (block exit ()
      (r (load i64 ctx 0))
      (return r))))
"#;

#[test]
fn counter_becomes_a_loop_parameter() {
    let (_, p, report) = run(COUNTER, &[cell(&load(COUNTER), 0, Type::I64, StateMask::OUTPUT)]);
    let f = &p.funcs[0];
    assert_eq!(report.promoted, 1);
    assert_eq!(report.phis, 1, "{}", print_program(&p));
    assert_eq!(report.loads_removed, 2);
    assert_eq!(report.stores_removed, 1);
    assert_eq!(report.stores_added, 1);
    // The loop block carries the count as a second parameter; no access in it.
    assert_eq!(f.blocks[1].params.len(), 2);
    assert_eq!(count_ops(f, 1, |o| is_cell_access(o, 0)), 0);
    // The exit stores the content before returning it.
    assert_eq!(count_ops(f, 2, |o| matches!(o, Op::Store { offset: 0, .. })), 1);
    assert_eq!(count_ops(f, 2, |o| matches!(o, Op::Load { .. })), 0);
    // The initial content is loaded once, in the entry.
    assert_eq!(count_ops(f, 0, |o| matches!(o, Op::Load { offset: 0, .. })), 1);
}

#[test]
fn verifier_rejects_a_dropped_write_back() {
    let cells = [cell(&load(COUNTER), 0, Type::I64, StateMask::OUTPUT)];
    let (p0, mut p, report) = run(COUNTER, &cells);
    let hosts = masks(&p);
    p.funcs[0].blocks[2].insts.retain(|i| !matches!(i.op, Op::Store { .. }));
    let m = p.mint();
    let err = verify_carried(&m, &p0.funcs[0], &p.funcs[0], &cells, &report, &hosts).unwrap_err();
    assert!(err.contains("returns with memory behind"), "{err}");
}

#[test]
fn verifier_rejects_a_wrong_edge_argument() {
    let cells = [cell(&load(COUNTER), 0, Type::I64, StateMask::OUTPUT)];
    let (p0, mut p, report) = run(COUNTER, &cells);
    let hosts = masks(&p);
    // The back edge passes the old count instead of the bumped one.
    let f = &mut p.funcs[0];
    let old = f.blocks[1].params[1];
    if let Terminator::CondBr { else_edge, .. } = &mut f.blocks[1].term {
        *else_edge.args.last_mut().unwrap() = old;
    }
    let m = p.mint();
    let err = verify_carried(&m, &p0.funcs[0], &p.funcs[0], &cells, &report, &hosts).unwrap_err();
    assert!(err.contains("predecessor's content"), "{err}");
}

#[test]
fn verifier_rejects_a_stale_read() {
    let cells = [cell(&load(COUNTER), 0, Type::I64, StateMask::OUTPUT)];
    let (p0, mut p, report) = run(COUNTER, &cells);
    let hosts = masks(&p);
    // The return reads the loop's parameter (the pre-bump count).
    let m = p.mint();
    let f = &mut p.funcs[0];
    let old = f.blocks[1].params[1];
    f.blocks[2].term = Terminator::Return(m.slice_from(&[old]));
    let err = verify_carried(&m, &p0.funcs[0], &p.funcs[0], &cells, &report, &hosts).unwrap_err();
    assert!(err.contains("stands in") || err.contains("terminator changed"), "{err}");
}

const HOSTED: &str = r#"
(program
  (host reader (ptr) void state=1)
  (host clean (ptr) void state=0)
  (fn f ((ctx ptr)) (i64)
    (block entry ((ctx ptr))
      (n (load i64 ctx 8))
      (env (load ptr ctx 16))
      (zero (const i64 0))
      (br loop zero))
    (block loop ((i i64))
      (c (load i64 ctx 0))
      (one (const i64 1))
      (c1 (iadd c one))
      ((store ctx 0 c1))
      ((call_host HOST env))
      (c2 (load i64 ctx 0))
      (two (const i64 2))
      (c3 (iadd c2 two))
      ((store ctx 0 c3))
      (i1 (iadd i one))
      (done (icmp sge i1 n))
      (condbr done (exit) (loop i1)))
    (block exit ()
      (r (load i64 ctx 0))
      (return r))))
"#;

#[test]
fn a_reading_host_gets_a_write_back_and_a_reload() {
    let src = HOSTED.replace("HOST", "reader");
    let (_, p, report) = run(&src, &[cell(&load(&src), 0, Type::I64, StateMask::OUTPUT)]);
    let f = &p.funcs[0];
    let text = print_program(&p);
    // One store before the call, one reload after it, nothing else in the loop.
    assert_eq!(count_ops(f, 1, |o| matches!(o, Op::Store { offset: 0, .. })), 1, "{text}");
    assert_eq!(count_ops(f, 1, |o| matches!(o, Op::Load { offset: 0, .. })), 1, "{text}");
    let pos = |pred: &dyn Fn(&Op) -> bool| f.blocks[1].insts.iter().position(|i| pred(&i.op)).unwrap();
    let call = pos(&|o| matches!(o, Op::CallHost { .. }));
    assert!(pos(&|o| matches!(o, Op::Store { .. })) < call);
    assert!(pos(&|o| matches!(o, Op::Load { offset: 0, .. })) > call);
    assert_eq!(report.stores_added, 2, "{text}");
}

#[test]
fn a_transparent_host_sees_nothing() {
    let src = HOSTED.replace("HOST", "clean");
    let (_, p, report) = run(&src, &[cell(&load(&src), 0, Type::I64, StateMask::OUTPUT)]);
    let f = &p.funcs[0];
    let text = print_program(&p);
    assert_eq!(count_ops(f, 1, |o| is_cell_access(o, 0)), 0, "{text}");
    assert_eq!(report.stores_added, 1);
    assert_eq!(report.phis, 1);
}

#[test]
fn a_host_of_another_class_is_transparent() {
    let src = HOSTED.replace("HOST", "reader");
    // The host reads OUTPUT; the cell is OPSTATE.
    let (_, p, _) = run(&src, &[cell(&load(&src), 0, Type::I64, StateMask::OPSTATE)]);
    assert_eq!(count_ops(&p.funcs[0], 1, |o| is_cell_access(o, 0)), 0, "{}", print_program(&p));
}

#[test]
fn a_read_only_cell_needs_no_phi() {
    let src = r#"
(program
  (fn f ((ctx ptr)) (i64)
    (block entry ((ctx ptr))
      (n (load i64 ctx 8))
      (zero (const i64 0))
      (br loop zero zero))
    (block loop ((i i64) (acc i64))
      (c (load i64 ctx 0))
      (acc1 (iadd acc c))
      (one (const i64 1))
      (i1 (iadd i one))
      (done (icmp sge i1 n))
      (condbr done (exit acc1) (loop i1 acc1)))
    (block exit ((a i64))
      (return a))))
"#;
    let (_, p, report) = run(src, &[cell(&load(src), 0, Type::I64, StateMask::OUTPUT)]);
    let f = &p.funcs[0];
    assert_eq!(report.phis, 0, "{}", print_program(&p));
    assert_eq!(report.stores_added, 0);
    assert_eq!(count_ops(f, 1, |o| is_cell_access(o, 0)), 0);
    // Hoisted to the entry.
    assert_eq!(count_ops(f, 0, |o| matches!(o, Op::Load { offset: 0, .. })), 1);
}

#[test]
fn a_narrow_cell_keeps_its_width() {
    let src = r#"
(program
  (fn f ((ctx ptr)) (i64)
    (block entry ((ctx ptr))
      (n (load i64 ctx 8))
      (zero (const i64 0))
      (br loop zero))
    (block loop ((i i64))
      (v (loadz i8 ctx 0))
      (one (const i64 1))
      (v1 (ior v one))
      (b (itrunc i8 v1))
      ((store ctx 0 b))
      (i1 (iadd i one))
      (done (icmp sge i1 n))
      (condbr done (exit) (loop i1)))
    (block exit ()
      (r (loadz i8 ctx 0))
      (return r))))
"#;
    let (_, p, report) = run(src, &[cell(&load(src), 0, Type::I8, StateMask::AGG)]);
    let f = &p.funcs[0];
    let text = print_program(&p);
    assert_eq!(report.phis, 1, "{text}");
    assert_eq!(f.value_type(f.blocks[1].params[1]), Type::I8);
    assert_eq!(count_ops(f, 1, |o| is_cell_access(o, 0)), 0, "{text}");
    assert_eq!(count_ops(f, 1, |o| matches!(o, Op::Conv { op: ConvOp::Zext, to: Type::I64, .. })), 1, "{text}");
    assert_eq!(count_ops(f, 2, |o| matches!(o, Op::Store { offset: 0, .. })), 1, "{text}");
}

#[test]
fn a_loaded_root_is_a_cell_too() {
    // The word lives behind a pointer loaded from the ctx (an OPSTATE word).
    let src = r#"
(program
  (fn f ((ctx ptr)) (i64)
    (block entry ((ctx ptr))
      (n (load i64 ctx 8))
      (zero (const i64 0))
      (br loop zero))
    (block loop ((i i64))
      (p (load ptr ctx 24))
      (c (load i64 p 16))
      (one (const i64 1))
      (c1 (iadd c one))
      (q (load ptr ctx 24))
      ((store q 16 c1))
      (i1 (iadd i one))
      (done (icmp sge i1 n))
      (condbr done (exit) (loop i1)))
    (block exit ()
      (return n))))
"#;
    let p0 = load(src);
    let cells = [Cell { root: Root::LoadedFrom { ptr: ctx(&p0), offset: 24 }, offset: 16, ty: Type::I64, class: StateMask::OPSTATE }];
    let (_, p, report) = run(src, &cells);
    let f = &p.funcs[0];
    let text = print_program(&p);
    assert_eq!(report.phis, 1, "{text}");
    // No word access and no pointer load left in the loop (dead, removed).
    assert_eq!(count_ops(f, 1, |o| matches!(o, Op::Load { .. } | Op::Store { .. })), 0, "{text}");
    // The exit loads the pointer and stores the word.
    assert_eq!(count_ops(f, 2, |o| matches!(o, Op::Store { offset: 16, .. })), 1, "{text}");
    assert_eq!(count_ops(f, 2, |o| matches!(o, Op::Load { ty: Type::Ptr, offset: 24, .. })), 1, "{text}");
}

#[test]
fn a_root_used_elsewhere_is_refused() {
    let src = r#"
(program
  (host take (ptr) void state=0)
  (fn f ((ctx ptr)) (i64)
    (block entry ((ctx ptr))
      (c (load i64 ctx 0))
      ((call_host take ctx))
      (return c))))
"#;
    let mut p = load(src);
    let cells = [cell(&p, 0, Type::I64, StateMask::OUTPUT)];
    let hosts = masks(&p);
    let report = promote(&mut p.funcs[0], &cells, &hosts, DEPTH).expect("promote");
    assert_eq!(report.promoted, 0);
    assert_eq!(report.refused.len(), 1);
}

#[test]
fn two_cells_two_parameters_in_declaration_order() {
    let src = r#"
(program
  (fn f ((ctx ptr)) (i64)
    (block entry ((ctx ptr))
      (n (load i64 ctx 16))
      (zero (const i64 0))
      (br loop zero))
    (block loop ((i i64))
      (a (load i64 ctx 0))
      (b (load i64 ctx 8))
      (one (const i64 1))
      (a1 (iadd a one))
      (b1 (iadd b a1))
      ((store ctx 0 a1))
      ((store ctx 8 b1))
      (i1 (iadd i one))
      (done (icmp sge i1 n))
      (condbr done (exit) (loop i1)))
    (block exit ()
      (r (load i64 ctx 8))
      (return r))))
"#;
    let p0 = load(src);
    let cells = [cell(&p0, 8, Type::I64, StateMask::OUTPUT), cell(&p0, 0, Type::I64, StateMask::OUTPUT)];
    let (_, p, report) = run(src, &cells);
    let f = &p.funcs[0];
    assert_eq!(report.phis, 2, "{}", print_program(&p));
    assert_eq!(f.blocks[1].params.len(), 3);
    // Cell 0 (offset 8) was declared first: its parameter comes first.
    assert_eq!(report.params[0].2, 0);
    assert_eq!(report.params[0].1, f.blocks[1].params[1]);
    assert_eq!(count_ops(f, 2, |o| matches!(o, Op::Store { .. })), 2);
}

/// A cell stored in the entry and read after `n` empty blocks: the read walks all `n` back.
fn chain(n: usize) -> String {
    let mut s = String::from("(program\n  (fn f ((ctx ptr)) (i64)\n    (block entry ((ctx ptr))\n      (one (const i64 1))\n      ((store ctx 0 one))\n      (br b0))\n");
    for i in 0..n {
        s += &format!("    (block b{i} ()\n      (br b{}))\n", i + 1);
    }
    s + &format!("    (block b{n} ()\n      (r (load i64 ctx 0))\n      (return r))))\n")
}

/// C6 (b): `promote` reads a content by walking predecessors, one recursion level per block. A chain
/// of blocks just short of the lowerer's depth promotes on an 8 MiB thread (a backend thread has
/// max_stack_depth plus 8 MiB); one past it is `Err(TooDeep)` (the lowerer keeps the function
/// unpromoted), where the unbounded walk took one level per block, however many.
#[test]
fn a_chain_of_blocks_promotes_or_stops_at_the_read_depth() {
    let run_on_8_mib = |n: usize| {
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(move || {
                let mut p = load(&chain(n));
                let cells = [cell(&p, 0, Type::I64, StateMask::OUTPUT)];
                let (hosts, f0) = (masks(&p), p.dup_func(Func(0)));
                let r = promote(&mut p.funcs[0], &cells, &hosts, DEPTH);
                if let Ok(report) = &r {
                    verify(&p).unwrap();
                    let m = p.mint();
                    verify_carried(&m, &f0, &p.funcs[0], &cells, report, &hosts).unwrap();
                }
                r.map(|report| report.promoted)
            })
            .unwrap()
            .join()
            .unwrap()
    };
    let max = DEPTH as usize;
    assert_eq!(run_on_8_mib(max - 8), Ok(1));
    assert_eq!(run_on_8_mib(max + 8), Err(TooDeep));
    assert_eq!(run_on_8_mib(100_000), Err(TooDeep));
}
