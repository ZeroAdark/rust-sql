//! Unreachable blocks emptied (D528): what a lowering emitted under a branch that never forms. A block no path from the
//! entry reaches keeps its number (block ids stay stable, so IR text and pins move only where code vanished) but loses its
//! parameters, instructions and edges: `(block bN () (unreachable))`. Its loop / likely hints go; a value it defined leaves
//! `remat` / `nowrap` (no reachable block can use it: its definition would not dominate the use).

use crate::*;

/// Every block no path from the entry reaches emptied to `unreachable`; false when every block is reached (`f` untouched).
/// Its tables live in `f`'s program's root (freed on return).
pub fn unreachable(f: &mut Function) -> bool {
    let m = f.mint();
    let n = f.blocks.len();
    let mut seen = m.vec_filled(false, n);
    let mut stack = m.vec_from(&[f.entry]);
    seen[f.entry.index()] = true;
    while let Some(b) = stack.pop() {
        for e in f.blocks[b.index()].term.edges() {
            if !core::mem::replace(&mut seen[e.target.index()], true) {
                stack.push(e.target);
            }
        }
    }
    if seen.iter().all(|&s| s) {
        return false;
    }
    let mut gone = m.vec_filled(false, f.values.len());
    for (bb, _) in f.blocks.iter_mut().zip(seen.iter()).filter(|(bb, s)| !**s && !(bb.params.is_empty() && bb.insts.is_empty() && matches!(bb.term, Terminator::Unreachable))) {
        bb.params.iter().chain(bb.insts.iter().filter_map(|x| x.result.as_ref())).for_each(|v| gone[v.index()] = true);
        (bb.params, bb.insts, bb.term) = (m.slice_from(&[]), m.vec(), Terminator::Unreachable);
    }
    let h = &mut f.hints;
    h.loop_headers.retain(|b| seen[b.index()]);
    h.likely.retain(|(b, _)| seen[b.index()]);
    h.remat.retain(|v| !gone[v.index()]);
    h.nowrap.retain(|v| !gone[v.index()]);
    true
}

#[cfg(test)]
mod tests {
    use crate::builder::ProgramBuilder;
    use crate::*;

    /// b1 (never entered) emptied in place: b2 keeps its number, its loop hint stays, b1's goes.
    #[test]
    fn an_unentered_block_is_emptied_in_place() {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("f", &[Type::I64], &[Type::I64]);
        {
            let mut b = pb.define(f);
            let x = b.params()[0];
            let (dead, live) = (b.create_block(&[Type::I64]), b.create_block(&[Type::I64]));
            b.br(live, &[x]);
            b.switch_to(dead);
            let c = b.iconst(Type::I64, 7);
            b.br(live, &[c]);
            b.mark_loop_header(dead);
            b.mark_loop_header(live);
            b.switch_to(live);
            let p = b.block_params(live)[0];
            b.ret(&[p]);
        }
        let mut p = pb.finish();
        let func = &mut p.funcs[f.index()];
        assert!(super::unreachable(func));
        assert_eq!(func.blocks.len(), 3);
        let d = &func.blocks[1];
        assert!(d.params.is_empty() && d.insts.is_empty() && matches!(d.term, Terminator::Unreachable));
        assert!(matches!(&func.blocks[0].term, Terminator::Br(e) if e.target == Block(2)));
        assert_eq!(func.hints.loop_headers, [Block(2)]);
        crate::verify::verify(&p).expect("verifies");
    }
}
