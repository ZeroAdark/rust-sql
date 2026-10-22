//! The cancel checks (lowerv4 plan P8; lowerv3 design §3.6, D211, D231): the frame emits every runtime check, never a
//! mechanism; `verify` is the back-edge lint over IR. The lowering's own stop between pipelines is `refusal::check_interrupts`.
use gravity_abi::ctx::hdr;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, Function, HostFlags, IntCond, Op, Type, Value};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

/// A check in the current block: a set cancel byte calls the kernels' poll (D3406, cold: `gravity_abi::ctx::poll`, the
/// owner's CHECK_FOR_INTERRUPTS on the byte's service bit), which branches to `l_cancel` or lets the code go on; a clear
/// byte continues in a new block, the one load and branch as before. The poll block is created after `l_on`, and the RPO
/// lays a block out before the one it rejoins: TPDE sinks it to its loop's end (`gravity_ir::KERNEL_POLL`, D3651).
pub fn check(b: &mut FunctionBuilder<'_>, ctx: Value, l_cancel: Block) {
    let byte = b.load(Type::Ptr, ctx, hdr::CANCEL as i32);
    let null = b.iconst(Type::Ptr, 0);
    let unbound = b.icmp(IntCond::Eq, byte, null);
    let l_read = b.create_block(&[]);
    let l_on = b.create_block(&[]);
    let l_poll = b.create_block(&[]);
    let cur = b.current();
    b.cond_br(unbound, (l_on, &[]), (l_read, &[]));
    b.set_likely(cur, false);
    b.switch_to(l_read);
    let v = b.load_zext(Type::I8, byte, 0);
    let zero = b.iconst(Type::I64, 0);
    let set = b.icmp(IntCond::Ne, v, zero);
    b.cond_br(set, (l_poll, &[]), (l_on, &[]));
    b.set_likely(l_read, false);
    b.mark_cold(l_poll);
    b.switch_to(l_poll);
    let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
    let poll = b.declare_host_once(gravity_ir::KERNEL_POLL, &[Type::Ptr], Some(Type::I64), gravity_kernels::contract::entry0::<gravity_kernels::host_abi::Poll> as *const () as usize, mem);
    let stop = b.call_host(poll, &[ctx], Some(Type::I64)).expect("poll returns a word");
    let zero = b.iconst(Type::I64, 0);
    let go = b.icmp(IntCond::Eq, stop, zero);
    b.cond_br(go, (l_on, &[]), (l_cancel, &[]));
    b.switch_to(l_on);
}

/// A checked loop header `h(params…)`: the check first, then the body block `(h, body)` with the same parameters.
pub fn header(b: &mut FunctionBuilder<'_>, ctx: Value, l_cancel: Block, params: &[Type], mcx: Mcx<'_>) -> (Block, Block) {
    let h = b.create_block(params);
    b.mark_loop_header(h);
    let body = b.create_block(params);
    let cur = b.current();
    b.switch_to(h);
    let p = mcx::to_vec_in(&b.block_params(h)[..], mcx);
    check(b, ctx, l_cancel);
    b.br(body, &p[..]);
    b.switch_to(cur);
    (h, body)
}

/// Every cycle of `f` that its entry reaches passes a block that loads `ctx.cancel` (a `check`) or repeats a `bounded`
/// header's own loop (a back edge into it: a row loop inside a checked window). The check need not head its cycle (D1028:
/// a retry checked on its full path keeps its row path unchecked); a cycle that only passes through a bounded header (the
/// loop enclosing it) still needs a check of its own. With the checking blocks and those back edges taken out, what is left
/// must be acyclic; `Err` names a block of a cycle left.
/// D3837: linear in the function: the bounded headers are a bitset, the exempt back edges a sorted list searched by halves
/// and the checking blocks found once (the walks are unchanged, so the verdict and the block it names are too); it was
/// O(edges x (exempt edges + bounded headers + a block's instructions)), 40 s of a 14-join program a cancel waited for.
pub fn verify(f: &Function, bounded: &[Block], mcx: Mcx<'_>) -> Result<(), Block> {
    let Some(&ctx) = f.blocks.get(f.entry.index()).and_then(|e| e.params.first()) else { return Ok(()) };
    let n = f.blocks.len();
    let succ: PgVec<'_, PgVec<'_, usize>> = f.blocks.iter().map(|b| b.term.edges().map(|e| e.target.index()).collect_in(mcx)).collect_in(mcx);
    let mut is_bounded = mcx::pg_vec![in mcx; false; n];
    for h in bounded {
        if let Some(x) = is_bounded.get_mut(h.index()) {
            *x = true;
        }
    }
    let (mut reached, mut exempt) = (mcx::pg_vec![in mcx; 0u8; n], PgVec::new_in(mcx));
    let mut into_bounded = |v: usize, w: usize| {
        if is_bounded[w] {
            exempt.push_in((v, w));
        }
        false
    };
    walk(&succ, f.entry.index()..f.entry.index() + 1, &|_| true, &|_, _| true, &mut into_bounded, &mut reached, mcx);
    exempt.sort_unstable();
    exempt.dedup();
    let loads: PgVec<'_, bool> = f.blocks.iter().map(|b| b.insts.iter().any(|i| matches!(i.op, Op::Load { ty: Type::Ptr, ptr, offset, .. } if ptr == ctx && offset == hdr::CANCEL as i32))).collect_in(mcx);
    let open = |v: usize| reached[v] != 0 && !loads[v];
    let mut state = mcx::pg_vec![in mcx; 0u8; n];
    match walk(&succ, 0..n, &open, &|v, w| exempt.binary_search(&(v, w)).is_err(), &mut |_, _| true, &mut state, mcx) {
        Some(w) => Err(Block(w as u32)),
        None => Ok(()),
    }
}

/// A depth-first walk from each of `roots` over the blocks `open` admits by the edges `edge` admits; `back(v, w)` at each
/// edge to a block on the walk's stack, the walk ending (with `w`) when it returns true. `seen`: 1 on the stack, 2 done.
fn walk<S: core::ops::Deref<Target = [usize]>>(succ: &[S], roots: core::ops::Range<usize>, open: &dyn Fn(usize) -> bool, edge: &dyn Fn(usize, usize) -> bool, back: &mut dyn FnMut(usize, usize) -> bool, seen: &mut [u8], mcx: Mcx<'_>) -> Option<usize> {
    for root in roots {
        if !open(root) || seen[root] != 0 {
            continue;
        }
        seen[root] = 1;
        let mut stack = mcx::pg_vec![in mcx; (root, 0usize)];
        while let Some(&mut (v, ref mut i)) = stack.last_mut() {
            let Some(&w) = succ[v].get(*i) else {
                seen[v] = 2;
                stack.pop();
                continue;
            };
            *i += 1;
            if !open(w) || !edge(v, w) {
                continue;
            }
            match seen[w] {
                0 => {
                    seen[w] = 1;
                    stack.push_in((w, 0));
                }
                1 if back(v, w) => return Some(w),
                _ => {}
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_ir::builder::ProgramBuilder;

    /// runs/sort.rs `RunEnd::append`'s retry: `try` tests for room, `full` runs the end's `on_full` and retries, `store`
    /// may find no room for a var copy and go to `full`; the check where `check_in` names (0 `full`, 1 `store`), `try`
    /// among the bounded blocks when `bounded`.
    fn retry(check_in: Option<usize>, bounded: bool) -> Result<(), Block> {
        let mut pb = ProgramBuilder::new();
        let func = pb.declare_func("t", &[Type::Ptr], &[Type::I64]);
        let l_try = {
            let mut b = pb.define(func);
            let ctx = b.params()[0];
            let (l_try, l_full, l_store, l_done, l_cancel) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
            let zero = b.iconst(Type::I64, 0);
            b.br(l_try, &[]);
            for (at, then, els) in [(l_try, l_full, l_store), (l_store, l_full, l_done)] {
                b.switch_to(at);
                if check_in == Some(1) && at == l_store {
                    check(&mut b, ctx, l_cancel);
                }
                let v = b.load(Type::I64, ctx, 8);
                let c = b.icmp(IntCond::Eq, v, zero);
                b.cond_br(c, (then, &[]), (els, &[]));
            }
            b.switch_to(l_full);
            if check_in == Some(0) {
                check(&mut b, ctx, l_cancel);
            }
            b.br(l_try, &[]);
            for l in [l_done, l_cancel] {
                b.switch_to(l);
                b.ret(&[zero]);
            }
            l_try
        };
        let p = pb.finish();
        let one = [l_try];
        let heads: &[Block] = if bounded { &one } else { &[] };
        verify(p.func(func), heads, mcx::MemoryContext::new("test").mcx())
    }

    /// A function whose blocks `0..succ.len()` branch to `succ[i]` (none: a return; two: on a loaded word), entered at 0;
    /// `checked` blocks check first, `bounded` ones are the verifier's bounded headers.
    fn graph(succ: &[&[usize]], checked: &[usize], bounded: &[usize]) -> Result<(), Block> {
        let (p, func, ls) = build(succ, checked);
        let bounded: Vec<Block> = bounded.iter().map(|&i| ls[i]).collect();
        verify(p.func(func), &bounded, mcx::MemoryContext::new("test").mcx())
    }

    /// `graph`'s function, built: the program, the function and its blocks `0..succ.len()`.
    fn build(succ: &[&[usize]], checked: &[usize]) -> (gravity_ir::Program, gravity_ir::Func, Vec<Block>) {
        let mut pb = ProgramBuilder::new();
        let func = pb.declare_func("t", &[Type::Ptr], &[Type::I64]);
        let ls = {
            let mut b = pb.define(func);
            let ctx = b.params()[0];
            let ls: Vec<Block> = succ.iter().map(|_| b.create_block(&[])).collect();
            let (l_cancel, zero) = (b.create_block(&[]), b.iconst(Type::I64, 0));
            b.br(ls[0], &[]);
            for (i, s) in succ.iter().enumerate() {
                b.switch_to(ls[i]);
                if checked.contains(&i) {
                    check(&mut b, ctx, l_cancel);
                }
                match *s {
                    [] => b.ret(&[zero]),
                    [t] => b.br(ls[*t], &[]),
                    [t, e] => {
                        let v = b.load(Type::I64, ctx, 8);
                        let c = b.icmp(IntCond::Eq, v, zero);
                        b.cond_br(c, (ls[*t], &[]), (ls[*e], &[]));
                    }
                    _ => unreachable!("two successors at most"),
                }
            }
            b.switch_to(l_cancel);
            b.ret(&[zero]);
            ls
        };
        (pb.finish(), func, ls)
    }

    /// The lint as it was before D3837 (linear scans per edge), kept as the reference the linear one must equal.
    fn verify_quadratic(f: &Function, bounded: &[Block]) -> Result<(), Block> {
        let Some(&ctx) = f.blocks.get(f.entry.index()).and_then(|e| e.params.first()) else { return Ok(()) };
        let n = f.blocks.len();
        let succ: Vec<Vec<usize>> = f.blocks.iter().map(|b| b.term.edges().map(|e| e.target.index()).collect()).collect();
        let (mut reached, mut exempt) = (vec![0u8; n], Vec::new());
        let mut into_bounded = |v: usize, w: usize| {
            if bounded.contains(&Block(w as u32)) {
                exempt.push((v, w));
            }
            false
        };
        let rig = mcx::MemoryContext::new("test");
        walk(&succ, f.entry.index()..f.entry.index() + 1, &|_| true, &|_, _| true, &mut into_bounded, &mut reached, rig.mcx());
        let loads = |v: usize| f.blocks[v].insts.iter().any(|i| matches!(i.op, Op::Load { ty: Type::Ptr, ptr, offset, .. } if ptr == ctx && offset == hdr::CANCEL as i32));
        let open = |v: usize| reached[v] != 0 && !loads(v);
        let mut state = vec![0u8; n];
        match walk(&succ, 0..n, &open, &|v, w| !exempt.contains(&(v, w)), &mut |_, _| true, &mut state, rig.mcx()) {
            Some(w) => Err(Block(w as u32)),
            None => Ok(()),
        }
    }

    /// D3837: the linear lint gives the quadratic one's verdict, and names the same block, on 4000 seeded random functions
    /// (1-12 blocks, 0-2 successors each, self loops, both arms of a branch to one block, random checked blocks and bounded
    /// headers), so the rewrite changed the cost only.
    #[test]
    fn the_linear_lint_equals_the_quadratic_one() {
        let mut x: u64 = 88_172_645_463_325_252; // xorshift64's own example seed
        let mut next = |m: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % m as u64) as usize
        };
        let (mut errs, mut oks) = (0, 0);
        let rig = mcx::MemoryContext::new("test");
        for _ in 0..4000 {
            let n = 1 + next(12);
            let succ: Vec<Vec<usize>> = (0..n).map(|_| (0..next(3)).map(|_| next(n)).collect()).collect();
            let checked: Vec<usize> = (0..n).filter(|_| next(4) == 0).collect();
            let heads: Vec<usize> = (0..n).filter(|_| next(3) == 0).collect();
            let refs: Vec<&[usize]> = succ.iter().map(|s| s.as_slice()).collect();
            let (p, func, ls) = build(&refs, &checked);
            let bounded: Vec<Block> = heads.iter().map(|&i| ls[i]).collect();
            let (got, want) = (verify(p.func(func), &bounded, rig.mcx()), verify_quadratic(p.func(func), &bounded));
            assert_eq!(got, want, "succ {succ:?} checked {checked:?} bounded {heads:?}");
            if got.is_err() { errs += 1 } else { oks += 1 }
        }
        assert!(errs > 400 && oks > 400, "both verdicts drawn often: {errs} refused, {oks} verified");
    }

    /// A nested loop: 1 the outer header, 2 the inner one (3 -> 2 its back edge, 4 -> 1 the outer's). The outer's check
    /// does not cover the inner cycle; the inner's own check, or its header bounded, does; a bounded inner header does not
    /// excuse the outer cycle, which passes through it.
    #[test]
    fn an_inner_cycle_that_misses_the_check_is_refused() {
        let nest: &[&[usize]] = &[&[1], &[2], &[3], &[2, 4], &[1, 5], &[]];
        assert!(graph(nest, &[1], &[]).is_err());
        assert_eq!(graph(nest, &[1, 2], &[]), Ok(()));
        assert_eq!(graph(nest, &[1], &[2]), Ok(()));
        assert!(graph(nest, &[], &[2]).is_err(), "the outer cycle still needs its check");
    }

    /// An irreducible cycle 2 <-> 3 entered at both from 1: a check in either block covers it, whichever the walk meets
    /// first (the back-edge form accepted only a check in the block its depth-first walk reached first); none is refused.
    #[test]
    fn an_irreducible_cycle_verifies_by_any_checked_block_on_it() {
        let irr: &[&[usize]] = &[&[1], &[2, 3], &[3, 4], &[2], &[]];
        assert!(graph(irr, &[], &[]).is_err());
        assert_eq!(graph(irr, &[2], &[]), Ok(()));
        assert_eq!(graph(irr, &[3], &[]), Ok(()));
    }

    /// D1028: the check on the full path alone closes both cycles through the retry (the row path takes none); a retry
    /// with no check, or one checked only on the store path, is refused; the bounded form verifies as before.
    #[test]
    fn a_retry_checked_on_its_full_path_verifies_and_an_unchecked_one_does_not() {
        assert_eq!(retry(Some(0), false), Ok(()));
        assert_eq!(retry(None, true), Ok(()));
        assert!(retry(None, false).is_err());
        assert!(retry(Some(1), false).is_err());
    }
}
