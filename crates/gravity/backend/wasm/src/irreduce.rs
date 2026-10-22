//! Irreducible control flow made reducible by the dispatch transform
//! (LLVM's `WebAssemblyFixIrreducibleControlFlow`, the step Ramsey's
//! translation assumes has run): a loop with several entries gets a
//! dispatch block whose `Switch` on a label parameter forwards to the
//! entry an edge meant, and every edge into an entry — from inside the
//! loop or outside — is rerouted through it with the label and the
//! entry's arguments (the other entries' parameters get zero dummies).
//! Regions are processed from the outside in: once every loop of a
//! region has one entry, each loop minus its header is a region. The
//! lowerer's hash-aggregate probe/rehash loops are the corpus's case.

use gravity_ir::*;

pub enum Fail {
    /// The function's entry block sits inside a multi-entry loop.
    EntryInLoop,
}

/// `Some(rewritten)` when the function needed the transform: a program of its own whose one function it is.
pub fn make_reducible(f: &Function) -> Result<Option<Program>, Fail> {
    let mut p = Program::new();
    let g = f.clone_in(&p.mint());
    p.funcs.push(g);
    let g = &mut p.funcs[0];
    let blocks = reachable(g);
    let mut changed = false;
    process_region(g, blocks, Some(f.entry), &mut changed)?;
    Ok(if changed { Some(p) } else { None })
}

fn reachable(f: &Function) -> Vec<Block> {
    let mut seen = vec![false; f.blocks.len()];
    let mut stack = vec![f.entry];
    seen[f.entry.index()] = true;
    let mut out = Vec::new();
    while let Some(b) = stack.pop() {
        out.push(b);
        for e in f.block(b).term.edges() {
            if !seen[e.target.index()] {
                seen[e.target.index()] = true;
                stack.push(e.target);
            }
        }
    }
    out
}

fn preds(f: &Function) -> Vec<Vec<Block>> {
    let mut p = vec![Vec::new(); f.blocks.len()];
    for b in reachable(f) {
        for e in f.block(b).term.edges() {
            p[e.target.index()].push(b);
        }
    }
    p
}

fn process_region(g: &mut Function, mut blocks: Vec<Block>, virtual_entry: Option<Block>, changed: &mut bool) -> Result<(), Fail> {
    stack_depth_core::check_stack_depth_or_panic();
    loop {
        let sccs = sccs(g, &blocks);
        let pr = preds(g);
        let mut fixed = false;
        for scc in &sccs {
            let in_scc = |b: Block| scc.contains(&b);
            let mut entries: Vec<Block> = scc.iter().copied().filter(|&b| virtual_entry == Some(b) || pr[b.index()].iter().any(|&p| !in_scc(p))).collect();
            entries.sort();
            if entries.len() > 1 {
                if entries.iter().any(|&e| virtual_entry == Some(e)) {
                    return Err(Fail::EntryInLoop);
                }
                let d = single_entry(g, &entries);
                blocks.push(d);
                *changed = true;
                fixed = true;
                break;
            }
        }
        if fixed {
            continue;
        }
        for scc in sccs {
            let in_scc = |b: Block| scc.contains(&b);
            let header = scc.iter().copied().find(|&b| virtual_entry == Some(b) || pr[b.index()].iter().any(|&p| !in_scc(p))).expect("a loop has an entry");
            let inner: Vec<Block> = scc.iter().copied().filter(|&b| b != header).collect();
            if !inner.is_empty() {
                process_region(g, inner, None, changed)?;
            }
        }
        return Ok(());
    }
}

/// Tarjan's algorithm over the subgraph `blocks`; the non-trivial components.
fn sccs(g: &Function, blocks: &[Block]) -> Vec<Vec<Block>> {
    let n = g.blocks.len();
    let mut member = vec![false; n];
    for &b in blocks {
        member[b.index()] = true;
    }
    struct St<'a> {
        g: &'a Function,
        member: Vec<bool>,
        index: Vec<u32>,
        low: Vec<u32>,
        on: Vec<bool>,
        stack: Vec<Block>,
        next: u32,
        out: Vec<Vec<Block>>,
    }
    fn visit(st: &mut St<'_>, v: Block) {
        stack_depth_core::check_stack_depth_or_panic();
        st.index[v.index()] = st.next;
        st.low[v.index()] = st.next;
        st.next += 1;
        st.stack.push(v);
        st.on[v.index()] = true;
        let succs: Vec<Block> = st.g.block(v).term.edges().map(|e| e.target).filter(|&w| st.member[w.index()]).collect();
        let mut self_loop = false;
        for w in succs {
            if w == v {
                self_loop = true;
            }
            if st.index[w.index()] == u32::MAX {
                visit(st, w);
                st.low[v.index()] = st.low[v.index()].min(st.low[w.index()]);
            } else if st.on[w.index()] {
                st.low[v.index()] = st.low[v.index()].min(st.index[w.index()]);
            }
        }
        if st.low[v.index()] == st.index[v.index()] {
            let mut comp = Vec::new();
            loop {
                let w = st.stack.pop().expect("scc stack");
                st.on[w.index()] = false;
                comp.push(w);
                if w == v {
                    break;
                }
            }
            if comp.len() > 1 || self_loop {
                comp.sort();
                st.out.push(comp);
            }
        }
    }
    let mut st = St { g, member, index: vec![u32::MAX; n], low: vec![0; n], on: vec![false; n], stack: Vec::new(), next: 0, out: Vec::new() };
    for &b in blocks {
        if st.index[b.index()] == u32::MAX {
            visit(&mut st, b);
        }
    }
    st.out
}

fn new_value(g: &mut Function, ty: Type) -> Value {
    g.values.push(ty);
    Value(g.values.len() as u32 - 1)
}

/// The dispatch block for `entries`; every edge into an entry now targets it.
fn single_entry(g: &mut Function, entries: &[Block]) -> Block {
    let d = Block(g.blocks.len() as u32);
    let label = new_value(g, Type::I32);
    let m = g.mint();
    let mut params = m.vec_from(&[label]);
    let mut slices: Vec<Vec<Value>> = Vec::with_capacity(entries.len());
    for &e in entries {
        let tys: Vec<Type> = g.block(e).params.iter().map(|&p| g.value_type(p)).collect();
        let slice: Vec<Value> = tys.iter().map(|&t| new_value(g, t)).collect();
        params.extend(slice.iter().copied());
        slices.push(slice);
    }
    let (last, rest) = entries.split_last().expect("two or more entries");
    let cases = m.slice_collect(rest.iter().enumerate().map(|(i, &e)| SwitchCase { value: i as u64, edge: m.edge(e, &slices[i]) }));
    let default = m.edge(*last, &slices[entries.len() - 1]);
    g.blocks.push(BasicBlock { params: IrSlice::from_vec(params), insts: m.vec(), term: Terminator::Switch { value: label, cases, default } });
    let entry_types: Vec<Vec<Type>> = slices.iter().map(|s| s.iter().map(|&v| g.value_type(v)).collect()).collect();
    for p in 0..g.blocks.len() - 1 {
        let Function { blocks, values, .. } = g;
        let BasicBlock { insts, term, .. } = &mut blocks[p];
        let mut fresh = |ty: Type| -> Value {
            values.push(ty);
            Value(values.len() as u32 - 1)
        };
        for edge in edges_mut(term) {
            let Some(i) = entries.iter().position(|&e| e == edge.target) else { continue };
            let lv = fresh(Type::I32);
            insts.push(Instruction { result: Some(lv), op: Op::Const { ty: Type::I32, bits: i as u64 } });
            let mut args = m.vec_from(&[lv]);
            for (j, tys) in entry_types.iter().enumerate() {
                if j == i {
                    args.extend(edge.args.iter().copied());
                    continue;
                }
                for &ty in tys {
                    if ty.is_vector() {
                        let z = fresh(ty.lane());
                        insts.push(Instruction { result: Some(z), op: Op::Const { ty: ty.lane(), bits: 0 } });
                        let v = fresh(ty);
                        insts.push(Instruction { result: Some(v), op: Op::VecSplat { ty, a: z } });
                        args.push(v);
                    } else {
                        let z = fresh(ty);
                        insts.push(Instruction { result: Some(z), op: Op::Const { ty, bits: 0 } });
                        args.push(z);
                    }
                }
            }
            edge.target = d;
            edge.args = IrSlice::from_vec(args);
        }
    }
    d
}

fn edges_mut(t: &mut Terminator) -> Vec<&mut Edge> {
    match t {
        Terminator::Br(e) => vec![e],
        Terminator::CondBr { then_edge, else_edge, .. } => vec![then_edge, else_edge],
        Terminator::Switch { cases, default, .. } => cases.iter_mut().map(|c| &mut c.edge).chain(core::iter::once(default)).collect(),
        Terminator::Return(_) | Terminator::Unreachable => Vec::new(),
    }
}
