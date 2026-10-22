//! Edge arity at `compile`: a branch that does not pass exactly its target
//! block's parameters is refused as an error in every build profile, not
//! only by the debug assertion in the view's `bind_args`. The shape is the
//! fused recursion's consumer walk (`recursive-fuse.md` §3, e2e group
//! Q90): a loop header `walk(w)` over an array header, left at the
//! array's end for a host call (`array_more`) and re-entered when the host
//! appended rows. The re-entry once branched to `walk` with no argument
//! (`tpde-phi-assert`): the walk's phi had no incoming value on that
//! predecessor. The well-formed program is the oracle: native and
//! interpreter agree on a crafted array the host grows in distinct
//! batches.

mod common;
use gravity_ir::builder::ProgramBuilder;
use gravity_ir::*;
use mcx::MemoryContext;

/// The host: `more(hdr) -> i64`. Cells: 0 POS, 8 LIMIT, 16 SUM, 24 the
/// batches left, 32.. the values. It appends a batch by moving LIMIT and
/// returns 1, or 0 when none is left.
extern "C" fn host_more(hdr: *mut i64) -> i64 {
    unsafe {
        let left = *hdr.add(3);
        if left == 0 {
            return 0;
        }
        *hdr.add(3) = left - 1;
        *hdr.add(1) += left; // batch sizes 3, 2, 1 for left = 3, 2, 1
        1
    }
}

fn walk_program(reenter_with_arg: bool) -> Program {
    let mut pb = ProgramBuilder::new();
    let more = pb.declare_host("more", &[Type::Ptr], Some(Type::I64), host_more as extern "C" fn(*mut i64) -> i64 as usize, HostFlags { touches_memory: true, ..Default::default() });
    let f = pb.declare_func("f", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let p = b.params()[0];
    let w0 = b.conv(ConvOp::PtrToInt, Type::I64, p);
    let walk = b.create_block(&[Type::I64]);
    let take = b.create_block(&[]);
    let ended = b.create_block(&[]);
    let fin = b.create_block(&[]);
    b.br(walk, &[w0]);
    b.switch_to(walk);
    let w = b.block_params(walk)[0];
    let arr = b.conv(ConvOp::IntToPtr, Type::Ptr, w);
    let pos = b.load(Type::I64, arr, 0);
    let lim = b.load(Type::I64, arr, 8);
    let have = b.icmp(IntCond::Slt, pos, lim);
    b.cond_br(have, (take, &[]), (ended, &[]));
    b.switch_to(take);
    let cell = b.ptr_add(arr, pos, 8, 32);
    let x = b.load(Type::I64, cell, 0);
    let sum = b.load(Type::I64, arr, 16);
    let sum1 = b.int_bin(IntBinOp::Add, sum, x);
    b.store(arr, 16, sum1);
    let one = b.iconst(Type::I64, 1);
    let pos1 = b.int_bin(IntBinOp::Add, pos, one);
    b.store(arr, 0, pos1);
    b.br(walk, &[w]);
    b.switch_to(ended);
    let st = b.call_host(more, &[arr], Some(Type::I64)).unwrap();
    let one = b.iconst(Type::I64, 1);
    let got = b.icmp(IntCond::Eq, st, one);
    if reenter_with_arg {
        b.cond_br(got, (walk, &[w]), (fin, &[]));
    } else {
        b.cond_br(got, (walk, &[]), (fin, &[]));
    }
    b.switch_to(fin);
    let r = b.load(Type::I64, arr, 16);
    b.ret(&[r]);
    pb.finish()
}

#[test]
fn an_edge_missing_the_blocks_argument_is_refused_at_compile() {
    let p = walk_program(false);
    let ctx = MemoryContext::new("gravity-tpde-test");
    let err = match gravity_tpde::compile(ctx.mcx(), &p) {
        Ok(_) => panic!("a phi with no incoming value on one predecessor compiled"),
        Err(e) => format!("{e:?}"),
    };
    assert!(err.contains("edge b3 -> b1 has 0 args, block has 1 params"), "{err}");
}

#[test]
fn the_walk_reentered_with_its_header_agrees_with_the_interpreter() {
    let p = walk_program(true);
    // 3 rows in the array, then batches of 3, 2 and 1 rows: the sum of
    // the first 9 values, only if every re-entry walks the same header.
    // Each backend gets its own array (the walk consumes it).
    let fresh = || {
        let mut hdr: Vec<i64> = vec![0, 3, 0, 3];
        hdr.extend((1..=12).map(|i| 100 * i + 7));
        hdr
    };
    let want: i64 = (1..=9).map(|i| 100 * i + 7).sum();
    let mut a = fresh();
    let interp = gravity_interp::Interp::new(&p).run(Func(0), &[a.as_mut_ptr() as u64]).unwrap();
    assert_eq!(interp, vec![want as u64]);
    assert_eq!(a[0], 9, "the interpreter consumed the array and all three batches");
    let mut b = fresh();
    let native = common::native(&p, Func(0), &[b.as_mut_ptr() as u64]);
    assert_eq!(native, interp, "native vs interpreter mismatch");
    assert_eq!(b[..4], a[..4], "the native walk consumed the array and all three batches");
}
