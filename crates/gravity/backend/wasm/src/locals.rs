//! D3228: a wasm local per live range, not per IR value: engines refuse a function with more than 50 000 locals (V8 and
//! wasmparser: 'local count too large'), so values of one wasm type whose live ranges never overlap share a local.
//! Liveness: a block's parameters are defined on its entry, an edge's arguments read at the end of the block it leaves
//! (`func::do_branch` pushes them all before it sets a parameter), and `func` reads an instruction's operands before it
//! sets its result. Colouring walks the dominator tree in preorder, so a value live into a block already has its local;
//! a value live somewhere its definition does not dominate (the irreducible dispatch's reroutes) keeps a local of its own.

use crate::cfg::Cfg;
use crate::val_type;
use gravity_ir::{Block, Function, Value};
use wasm_encoder::ValType;

/// The local of each value (the entry block's parameters are the wasm parameters `0..np`) and the types of the locals
/// declared after the parameters.
pub(crate) fn assign(f: &Function, cfg: &Cfg) -> (Vec<u32>, Vec<ValType>) {
    let nv = f.values.len();
    let nb = f.blocks.len();
    let entry = f.entry;
    let np = f.block(entry).params.len();
    let none = u32::MAX;

    let mut def_block = vec![none; nv];
    let mut preds: Vec<Vec<u32>> = vec![Vec::new(); nb];
    let mut uses: Vec<(u32, u32)> = Vec::new();
    let mut ops: Vec<Value> = Vec::new();
    for &b in &cfg.rpo {
        let bb = f.block(b);
        let bi = b.index() as u32;
        for &p in &bb.params {
            def_block[p.index()] = bi;
        }
        for inst in &bb.insts {
            ops.clear();
            inst.op.operands_into(&mut ops);
            uses.extend(ops.iter().map(|o| (o.index() as u32, bi)));
            if let Some(r) = inst.result {
                def_block[r.index()] = bi;
            }
        }
        ops.clear();
        bb.term.operands_into(&mut ops);
        uses.extend(ops.iter().map(|o| (o.index() as u32, bi)));
        for e in bb.term.edges() {
            uses.extend(e.args.iter().map(|a| (a.index() as u32, bi)));
            preds[e.target.index()].push(bi);
        }
    }
    uses.sort_unstable();
    uses.dedup();

    let mut live_in: Vec<Vec<u32>> = vec![Vec::new(); nb];
    let mut live_out: Vec<Vec<u32>> = vec![Vec::new(); nb];
    let (mut in_mark, mut out_mark) = (vec![none; nb], vec![none; nb]);
    let mut work: Vec<u32> = Vec::new();
    for &(v, b) in &uses {
        let d = def_block[v as usize];
        if d == b || in_mark[b as usize] == v {
            continue;
        }
        in_mark[b as usize] = v;
        live_in[b as usize].push(v);
        work.push(b);
        while let Some(x) = work.pop() {
            for &p in &preds[x as usize] {
                if out_mark[p as usize] != v {
                    out_mark[p as usize] = v;
                    live_out[p as usize].push(v);
                }
                if p != d && in_mark[p as usize] != v {
                    in_mark[p as usize] = v;
                    live_in[p as usize].push(v);
                    work.push(p);
                }
            }
        }
    }

    // dominator-tree preorder intervals: a dominates b iff pre[a] <= pre[b] && post[b] <= post[a]
    let (mut pre, mut post) = (vec![0u32; nb], vec![0u32; nb]);
    let mut order: Vec<Block> = Vec::with_capacity(cfg.rpo.len());
    let mut dfs: Vec<(Block, bool)> = vec![(entry, false)];
    let mut clock = 0u32;
    while let Some((b, done)) = dfs.pop() {
        clock += 1;
        if done {
            post[b.index()] = clock;
            continue;
        }
        pre[b.index()] = clock;
        order.push(b);
        dfs.push((b, true));
        dfs.extend(cfg.children[b.index()].iter().rev().map(|&c| (c, false)));
    }

    let mut loc = vec![none; nv];
    let mut types: Vec<ValType> = Vec::with_capacity(np + 64);
    for (i, &p) in f.block(entry).params.iter().enumerate() {
        loc[p.index()] = i as u32;
        types.push(val_type(f.value_type(p)));
    }
    let mut own = vec![false; nv];
    for &b in &order {
        for &v in &live_in[b.index()] {
            let d = def_block[v as usize];
            let dominated = d != none && pre[d as usize] <= pre[b.index()] && post[b.index()] <= post[d as usize];
            if !dominated && !own[v as usize] {
                own[v as usize] = true;
                loc[v as usize] = types.len() as u32;
                types.push(val_type(f.values[v as usize]));
            }
        }
    }
    let mut reserved = vec![false; types.len()];
    for v in 0..nv {
        if own[v] {
            reserved[loc[v] as usize] = true;
        }
    }

    let class = |t: ValType| match t {
        ValType::I32 => 0,
        ValType::I64 => 1,
        ValType::F32 => 2,
        ValType::F64 => 3,
        _ => 4,
    };
    let take = |v: Value, free: &mut [Vec<u32>; 5], types: &mut Vec<ValType>, loc: &mut Vec<u32>| {
        if own[v.index()] {
            return;
        }
        let t = val_type(f.value_type(v));
        loc[v.index()] = free[class(t)].pop().unwrap_or_else(|| {
            types.push(t);
            types.len() as u32 - 1
        });
    };
    let give = |v: u32, free: &mut [Vec<u32>; 5], types: &Vec<ValType>, loc: &Vec<u32>| {
        if !own[v as usize] {
            let l = loc[v as usize];
            free[class(types[l as usize])].push(l);
        }
    };
    let mut free: [Vec<u32>; 5] = Default::default();
    let mut taken: Vec<u32> = Vec::new();
    let mut seen = vec![none; nv];
    let mut dying: Vec<Vec<u32>> = Vec::new();
    for (visit, &b) in order.iter().enumerate() {
        let visit = visit as u32;
        let bb = f.block(b);
        let bi = b.index();

        taken.resize(types.len(), none);
        for &v in &live_in[bi] {
            debug_assert!(loc[v as usize] != none, "a live-in value its definition dominates is coloured first");
            taken[loc[v as usize] as usize] = visit;
        }
        if b == entry {
            for &p in &bb.params {
                taken[loc[p.index()] as usize] = visit;
            }
        }
        for list in free.iter_mut() {
            list.clear();
        }
        for l in (0..types.len()).rev() {
            if taken[l] != visit && !reserved.get(l).copied().unwrap_or(false) {
                free[class(types[l])].push(l as u32);
            }
        }

        // an operand dies at the last instruction reading it unless the terminator, an edge or a successor needs it
        for &v in &live_out[bi] {
            seen[v as usize] = visit;
        }
        ops.clear();
        bb.term.operands_into(&mut ops);
        for e in bb.term.edges() {
            ops.extend_from_slice(&e.args);
        }
        for o in &ops {
            seen[o.index()] = visit;
        }
        dying.resize_with(dying.len().max(bb.insts.len()), Vec::new);
        for (k, inst) in bb.insts.iter().enumerate().rev() {
            dying[k].clear();
            ops.clear();
            inst.op.operands_into(&mut ops);
            for o in &ops {
                if seen[o.index()] != visit {
                    seen[o.index()] = visit;
                    dying[k].push(o.index() as u32);
                }
            }
        }

        if b != entry {
            for &p in &bb.params {
                take(p, &mut free, &mut types, &mut loc);
            }
        }
        // parameters are set together; one nothing reads frees its local only after all are placed
        for &p in &bb.params {
            if seen[p.index()] != visit {
                give(p.index() as u32, &mut free, &types, &loc);
            }
        }
        for (k, inst) in bb.insts.iter().enumerate() {
            for &o in &dying[k] {
                give(o, &mut free, &types, &loc);
            }
            if let Some(r) = inst.result {
                take(r, &mut free, &mut types, &mut loc);
                if seen[r.index()] != visit {
                    give(r.index() as u32, &mut free, &types, &loc);
                }
            }
        }
    }

    // values of unreachable blocks are never emitted
    for v in 0..nv {
        if loc[v] == none {
            let t = val_type(f.values[v]);
            loc[v] = types.iter().position(|&x| x == t).map(|l| l as u32).unwrap_or_else(|| {
                types.push(t);
                types.len() as u32 - 1
            });
        }
    }
    (loc, types.split_off(np))
}
