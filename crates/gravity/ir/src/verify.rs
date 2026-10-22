//! Structural, type and dominance checks. Used by tests, the text loader and
//! lowerv4's every lowering (a finding refuses the statement as a bug).

use crate::*;
use alloc::format;
use alloc::string::String; // alloc: Cold
use mcx::{Mcx, PgVec, SendRoot};

/// The checks' tables for a caller with no context to give (tests, tools, the text loader): an mcx root of their own per
/// call (its bytes on the calling thread's owner), gone with the call. A lowering gives its planning context (`verify_in`):
/// no root context is made outside mem on the product path (C14).
fn tables() -> SendRoot {
    SendRoot::new("GravityVerify")
}

fn filled<T: Clone>(m: Mcx<'_>, x: T, n: usize) -> PgVec<'_, T> {
    let mut v = PgVec::with_capacity_in(n, m);
    v.resize(n, x);
    v
}

#[derive(Debug, PartialEq, Eq)]
pub struct VerifyError {
    pub func: String, // alloc: Cold
    pub message: String, // alloc: Cold
}

/// Where a finding is, the label its message starts with. Formatted only
/// when a message is: a label `format!`ed per instruction and definition
/// before any error existed was 25 of verify's 46 µs per lowering
/// (M1-WRITE's compile profile, 09-24). `verify_lowered` finds the failing
/// block by the label's first word.
#[derive(Clone, Copy)]
enum Site {
    Params(usize),
    Inst(usize, usize),
    Term(usize),
    Edge(usize, u32),
}

impl core::fmt::Display for Site {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Site::Params(b) => write!(f, "b{b} params"),
            Site::Inst(b, i) => write!(f, "b{b} inst {i}"),
            Site::Term(b) => write!(f, "b{b} terminator"),
            Site::Edge(b, t) => write!(f, "b{b} terminator: edge to b{t}"),
        }
    }
}

/// The structural subset of `verify`, cheap enough to run on every lowered
/// program in every build profile: blocks and values in range, every
/// value defined exactly once (block parameters and instruction results),
/// every edge naming an existing block other than the entry and passing
/// exactly as many arguments as the block has parameters. A backend binds
/// a block's parameters to an edge's arguments by position (the
/// interpreter's parallel copy, the TPDE view's phis), so an arity
/// mismatch is a program no backend can run faithfully. The entry block
/// has no incoming edge: its parameters are the function's arguments,
/// never phis (the TPDE view gives the entry none, so an edge into it has
/// nothing to bind its arguments to: D296's census, a backend panic); a
/// loop starts at a block of its own. Types and dominance (every use
/// dominated by its definition: a use its definition does not dominate
/// reads whatever the register or slot holds on the paths that skipped
/// the definition — the interpreter's unset slot reads as 0, the native
/// code's as garbage: the register-state lane's hang, D296's other
/// backend item) are `verify`'s business.
pub fn verify_structure(program: &Program) -> Result<(), VerifyError> {
    verify_structure_in(program, tables().mcx())
}

/// `verify_structure` with its tables in `m`.
pub fn verify_structure_in(program: &Program, m: Mcx<'_>) -> Result<(), VerifyError> {
    for f in program.funcs.iter() {
        verify_function_structure(f, m).map_err(|m| VerifyError { func: String::from(f.name.as_str()), message: m })?; // alloc: Cold
    }
    Ok(())
}

fn verify_function_structure(f: &Function, m: Mcx<'_>) -> Result<(), String> { // alloc: Cold
    let nv = f.values.len();
    let mut defined = filled(m, false, nv);
    let mut define = |v: Value, what: Site| -> Result<(), String> { // alloc: Cold
        if v.index() >= nv {
            return Err(format!("{what}: value v{} out of range", v.0)); // alloc: Cold
        }
        if defined[v.index()] {
            return Err(format!("{what}: value v{} defined twice", v.0)); // alloc: Cold
        }
        defined[v.index()] = true;
        Ok(())
    };
    if f.blocks.is_empty() {
        return Err(String::from("no blocks")); // alloc: Cold
    }
    if f.entry.index() >= f.blocks.len() {
        return Err(format!("entry b{} out of range", f.entry.0)); // alloc: Cold
    }
    for (bi, b) in f.blocks.iter().enumerate() {
        for &v in b.params.iter() {
            define(v, Site::Params(bi))?;
        }
        for (ii, inst) in b.insts.iter().enumerate() {
            if let Some(r) = inst.result {
                define(r, Site::Inst(bi, ii))?;
            }
        }
    }
    for (bi, b) in f.blocks.iter().enumerate() {
        for e in b.term.edges() {
            let Some(tb) = f.blocks.get(e.target.index()) else {
                return Err(format!("b{bi} terminator: edge to b{} out of range", e.target.0)); // alloc: Cold
            };
            if e.target == f.entry {
                return Err(format!("b{bi} terminator: edge to b{}, the entry block (its parameters are the function's arguments)", e.target.0)); // alloc: Cold
            }
            if tb.params.len() != e.args.len() {
                return Err(format!("b{bi} terminator: edge to b{} has {} args, block has {} params", e.target.0, e.args.len(), tb.params.len())); // alloc: Cold
            }
            for &a in e.args.iter() {
                if a.index() >= nv {
                    return Err(format!("b{bi} terminator: edge to b{} passes v{} out of range", e.target.0, a.0)); // alloc: Cold
                }
            }
        }
    }
    let _ = defined;
    Ok(())
}

/// Every use is dominated by its definition. Dominators by the iterative
/// intersection over a reverse postorder from the entry; blocks the entry
/// does not reach are not checked (nothing runs them: TPDE's analyzer lays
/// out the same reverse postorder and drops them, the interpreter never
/// branches there). Within a block the definition must come first (block
/// parameters are defined at the top). A use of a value nothing defines is
/// named, never a panic. Flat tables throughout (successors, predecessors,
/// the dominator tree as preorder intervals): one query per use is two
/// compares, and the only allocations are a handful of vectors per function.
fn verify_function_dominance(f: &Function, m: Mcx<'_>) -> Result<(), String> { // alloc: Cold
    const NONE: u32 = u32::MAX;
    let nb = f.blocks.len();
    let nv = f.values.len();
    // Where each value is defined: (block, position); params at 0, the
    // i-th instruction at i + 1.
    let mut def = filled(m, (NONE, 0u32), nv);
    for (bi, b) in f.blocks.iter().enumerate() {
        for &v in b.params.iter() {
            def[v.index()] = (bi as u32, 0);
        }
        for (ii, inst) in b.insts.iter().enumerate() {
            if let Some(r) = inst.result {
                def[r.index()] = (bi as u32, ii as u32 + 1);
            }
        }
    }
    // Successors, flat: succ[start[b] .. start[b + 1]].
    let mut start: PgVec<'_, u32> = PgVec::with_capacity_in(nb + 1, m);
    let mut succ: PgVec<'_, u32> = PgVec::with_capacity_in(nb * 2, m);
    for b in f.blocks.iter() {
        start.push(succ.len() as u32);
        for e in b.term.edges() {
            succ.push(e.target.0);
        }
    }
    start.push(succ.len() as u32);
    // Reverse postorder from the entry: order[b] is b's position, NONE when
    // the entry does not reach it.
    let entry = f.entry.0;
    let mut order = filled(m, NONE, nb);
    let mut rpo: PgVec<'_, u32> = PgVec::with_capacity_in(nb, m);
    {
        let mut seen = filled(m, false, nb);
        let mut stack: PgVec<'_, (u32, u32)> = PgVec::with_capacity_in(nb, m);
        stack.push((entry, start[entry as usize]));
        seen[entry as usize] = true;
        while let Some(top) = stack.last_mut() {
            let (b, i) = *top;
            if i < start[b as usize + 1] {
                top.1 += 1;
                let s = succ[i as usize];
                if !seen[s as usize] {
                    seen[s as usize] = true;
                    stack.push((s, start[s as usize]));
                }
            } else {
                rpo.push(b);
                stack.pop();
            }
        }
        rpo.reverse();
    }
    for (i, &b) in rpo.iter().enumerate() {
        order[b as usize] = i as u32;
    }
    // Immediate dominators (Cooper, Harvey, Kennedy), by RPO position: the
    // predecessors of a block in the pass are its reachable ones.
    let n = rpo.len();
    let mut pstart = filled(m, 0u32, n + 1);
    for &b in rpo.iter() {
        for &s in &succ[start[b as usize] as usize..start[b as usize + 1] as usize] {
            pstart[order[s as usize] as usize + 1] += 1;
        }
    }
    for i in 0..n {
        pstart[i + 1] += pstart[i];
    }
    let mut pred = filled(m, 0u32, pstart[n] as usize);
    {
        let mut fill = PgVec::with_capacity_in(n + 1, m);
        fill.extend_from_slice(&pstart);
        for (bo, &b) in rpo.iter().enumerate() {
            for &s in &succ[start[b as usize] as usize..start[b as usize + 1] as usize] {
                let so = order[s as usize] as usize;
                pred[fill[so] as usize] = bo as u32;
                fill[so] += 1;
            }
        }
    }
    let mut idom = filled(m, NONE, n);
    idom[0] = 0;
    let mut changed = true;
    while changed {
        changed = false;
        for b in 1..n {
            let mut new = NONE;
            for &p in &pred[pstart[b] as usize..pstart[b + 1] as usize] {
                if idom[p as usize] == NONE {
                    continue;
                }
                new = if new == NONE {
                    p
                } else {
                    let (mut x, mut y) = (p, new);
                    while x != y {
                        while x > y {
                            x = idom[x as usize];
                        }
                        while y > x {
                            y = idom[y as usize];
                        }
                    }
                    x
                };
            }
            if new != idom[b] {
                idom[b] = new;
                changed = true;
            }
        }
    }
    // The dominator tree as preorder intervals: a dominates b iff
    // pre[a] <= pre[b] < pre[a] + size[a] (RPO positions throughout).
    let mut kstart = filled(m, 0u32, n + 1);
    for b in 1..n {
        kstart[idom[b] as usize + 1] += 1;
    }
    for i in 0..n {
        kstart[i + 1] += kstart[i];
    }
    let mut kids = filled(m, 0u32, n.saturating_sub(1));
    {
        let mut fill = PgVec::with_capacity_in(n + 1, m);
        fill.extend_from_slice(&kstart);
        for b in 1..n {
            let d = idom[b] as usize;
            kids[fill[d] as usize] = b as u32;
            fill[d] += 1;
        }
    }
    let mut pre = filled(m, 0u32, n);
    let mut size = filled(m, 1u32, n);
    {
        let mut next = 0u32;
        let mut stack: PgVec<'_, (u32, u32)> = PgVec::with_capacity_in(n, m);
        if n > 0 {
            pre[0] = 0;
            next = 1;
            stack.push((0, kstart[0]));
        }
        while let Some(top) = stack.last_mut() {
            let (b, i) = *top;
            if i < kstart[b as usize + 1] {
                top.1 += 1;
                let c = kids[i as usize];
                pre[c as usize] = next;
                next += 1;
                stack.push((c, kstart[c as usize]));
            } else {
                stack.pop();
                if let Some(parent) = stack.last() {
                    size[parent.0 as usize] += size[b as usize];
                }
            }
        }
    }
    let check = |v: Value, block: u32, position: u32, what: Site| -> Result<(), String> { // alloc: Cold
        let (db, dp) = if v.index() < nv { def[v.index()] } else { (NONE, 0) };
        if db == NONE {
            return Err(format!("{what}: v{} is never defined", v.0)); // alloc: Cold
        }
        let ok = if db == block {
            dp < position
        } else {
            let (a, u) = (order[db as usize], order[block as usize]);
            a != NONE && pre[a as usize] <= pre[u as usize] && pre[u as usize] < pre[a as usize] + size[a as usize]
        };
        if !ok {
            return Err(format!("{what}: v{} is defined in b{db}, which does not dominate the use", v.0)); // alloc: Cold
        }
        Ok(())
    };
    for &b in rpo.iter() {
        let blk = &f.blocks[b as usize];
        for (ii, inst) in blk.insts.iter().enumerate() {
            let mut r = Ok(());
            inst.op.for_each_operand(|v| {
                if r.is_ok() {
                    r = check(v, b, ii as u32 + 1, Site::Inst(b as usize, ii));
                }
            });
            r?;
        }
        let tp = blk.insts.len() as u32 + 1;
        let mut r = Ok(());
        blk.term.for_each_operand(|v| {
            if r.is_ok() {
                r = check(v, b, tp, Site::Term(b as usize));
            }
        });
        r?;
        for e in blk.term.edges() {
            for &a in e.args.iter() {
                check(a, b, tp, Site::Edge(b as usize, e.target.0))?;
            }
        }
    }
    Ok(())
}

/// The check on every program a lowerer emits (`gravity_pipeline_lower`,
/// `tpde-phi-assert`): the structural checks in every build profile (a
/// backend binds block parameters to edge arguments by position, so a
/// mismatch is a program no backend can run faithfully), the full
/// verifier (types too) on dev builds — a type finding is a lowering to
/// fix, not a miscompile today (the backends treat `i64` and `ptr`
/// alike), and the dev-profile e2e is where it shows. The error is the
/// text for the caller's panic: the function, the message, the failing
/// block (the message starts with its name) and, when `dump` is set
/// (`GRAVITY_VERIFY_DUMP=1`), the whole program. Dominance is part of
/// `verify` (main 09-24), so the old `GRAVITY_VERIFY_DOMINANCE` opt-in is gone.
pub fn verify_lowered(program: &Program, dump: bool) -> Result<(), String> { // alloc: Cold
    let verified = verify_structure(program);
    #[cfg(debug_assertions)]
    let verified = verified.and_then(|()| verify(program));
    let Err(e) = verified else { return Ok(()) };
    let text = crate::text::print_program(program);
    let block = e.message.split(' ').next().and_then(|b| {
        let start = text.find(&format!("(block {b} "))?; // alloc: Cold
        let end = text[start + 1..].find("\n    (block ").map_or(text.len(), |i| start + 1 + i);
        Some(&text[start..end])
    });
    Err(format!("fn {}: {}\n{}\n{}", e.func, e.message, block.unwrap_or(""), if dump { text.as_str() } else { "" })) // alloc: Cold
}

/// The whole verifier: `verify_structure`, then per function the types and
/// SSA dominance (`verify_dominance`'s check). lowerv4 runs it on every
/// lowering in every build profile (frame::program::verify: a finding is a
/// `Reason::Bug` refusal, never a program handed to a backend).
pub fn verify(program: &Program) -> Result<(), VerifyError> {
    verify_in(program, tables().mcx())
}

/// `verify` with its tables in `m`: a lowering's planning context (lowerv4 frame::program::verify), so they are the
/// statement's plan bytes by construction, freed as each check ends.
pub fn verify_in(program: &Program, m: Mcx<'_>) -> Result<(), VerifyError> {
    verify_structure_in(program, m)?;
    for f in program.funcs.iter() {
        verify_function(program, f, m).and_then(|()| verify_function_dominance(f, m)).map_err(|m| VerifyError { func: String::from(f.name.as_str()), message: m })?; // alloc: Cold
    }
    Ok(())
}

/// SSA dominance: every use dominated by its definition (the defining
/// block dominates the using block, or is the same block with the
/// definition first). A use its definition does not dominate reads
/// whatever the register or slot holds on the paths that skipped the
/// definition: the interpreter's unset slot reads as 0, the native code's
/// as garbage (the register-state lane's hang under TPDE). Part of
/// `verify` since main 09-24 (the lowerings that carried such uses,
/// `gravity_pipeline_lower`'s, are gone, and none of the plan fixtures v4
/// lowers has one); this is the structural checks and dominance alone,
/// without the types, for a test that builds ill-typed programs.
pub fn verify_dominance(program: &Program) -> Result<(), VerifyError> {
    let mut cx = tables();
    let m = cx.mcx();
    verify_structure_in(program, m)?;
    for f in program.funcs.iter() {
        verify_function_dominance(f, m).map_err(|m| VerifyError { func: String::from(f.name.as_str()), message: m })?; // alloc: Cold
    }
    Ok(())
}

fn verify_function(p: &Program, f: &Function, m: Mcx<'_>) -> Result<(), String> { // alloc: Cold
    let nv = f.values.len();
    let mut defined = filled(m, false, nv);
    let mut define = |v: Value, what: Site| -> Result<(), String> { // alloc: Cold
        if v.index() >= nv {
            return Err(format!("{what}: value v{} out of range", v.0)); // alloc: Cold
        }
        if defined[v.index()] {
            return Err(format!("{what}: value v{} defined twice", v.0)); // alloc: Cold
        }
        defined[v.index()] = true;
        Ok(())
    };
    if f.blocks.is_empty() {
        return Err(String::from("no blocks")); // alloc: Cold
    }
    if f.entry.index() >= f.blocks.len() {
        return Err(String::from("entry block out of range")); // alloc: Cold
    }
    if !f.blocks[f.entry.index()].params.iter().map(|&v| f.values[v.index()]).eq(f.params.iter().copied()) {
        return Err(String::from("entry block parameters do not match function parameters")); // alloc: Cold
    }
    if f.params.iter().chain(&f.returns).any(|t| t.is_vector()) {
        return Err(String::from("vector types never cross a function boundary")); // alloc: Cold
    }
    for (bi, b) in f.blocks.iter().enumerate() {
        for &v in b.params.iter() {
            define(v, Site::Params(bi))?;
        }
        for (ii, inst) in b.insts.iter().enumerate() {
            if let Some(r) = inst.result {
                define(r, Site::Inst(bi, ii))?;
            }
        }
    }
    // Types (dominance: `verify_function_dominance`, which `verify` runs after).
    let ty = |v: Value| -> Result<Type, String> { // alloc: Cold
        f.values.get(v.index()).copied().ok_or_else(|| format!("v{} out of range", v.0)) // alloc: Cold
    };
    let expect = |v: Value, t: Type, what: Site| -> Result<(), String> { // alloc: Cold
        let got = ty(v)?;
        if got != t {
            return Err(format!("{what}: v{} is {got:?}, expected {t:?}", v.0)); // alloc: Cold
        }
        Ok(())
    };
    let same = |a: Value, b: Value, what: Site| -> Result<Type, String> { // alloc: Cold
        let (ta, tb) = (ty(a)?, ty(b)?);
        if ta != tb {
            return Err(format!("{what}: v{} is {ta:?} but v{} is {tb:?}", a.0, b.0)); // alloc: Cold
        }
        Ok(ta)
    };
    let check_edge = |e: &Edge, what: Site| -> Result<(), String> { // alloc: Cold
        let tb = f.blocks.get(e.target.index()).ok_or_else(|| format!("{what}: b{} out of range", e.target.0))?; // alloc: Cold
        if tb.params.len() != e.args.len() {
            return Err(format!("{what}: edge to b{} has {} args, block has {} params", e.target.0, e.args.len(), tb.params.len())); // alloc: Cold
        }
        for (&a, &p) in e.args.iter().zip(tb.params.iter()) {
            same(a, p, what)?;
        }
        Ok(())
    };
    for (bi, b) in f.blocks.iter().enumerate() {
        for (ii, inst) in b.insts.iter().enumerate() {
            let what = Site::Inst(bi, ii);
            let result_ty = inst.result.map(ty).transpose()?;
            let need = |t: Option<Type>| -> Result<(), String> { // alloc: Cold
                if result_ty != t {
                    return Err(format!("{what}: result type {result_ty:?}, expected {t:?}")); // alloc: Cold
                }
                Ok(())
            };
            match &inst.op {
                Op::Const { ty: t, .. } => {
                    if t.is_vector() {
                        return Err(format!("{what}: no vector constants (use vsplat)")); // alloc: Cold
                    }
                    need(Some(*t))?
                }
                Op::IntBin { op, a, b } => {
                    let t = same(*a, *b, what)?;
                    let logical = matches!(op, IntBinOp::And | IntBinOp::Or | IntBinOp::Xor);
                    if !(t.is_int() || (logical && t == Type::Bool)) {
                        return Err(format!("{what}: int op on {t:?}")); // alloc: Cold
                    }
                    if matches!(op, IntBinOp::Smulh | IntBinOp::Umulh | IntBinOp::Rotl) && t != Type::I64 {
                        return Err(format!("{what}: {op:?} on {t:?}")); // alloc: Cold
                    }
                    need(Some(t))?;
                }
                Op::IntOvf { a, b, .. } => {
                    let t = same(*a, *b, what)?;
                    if !t.is_int() {
                        return Err(format!("{what}: int op on {t:?}")); // alloc: Cold
                    }
                    need(Some(Type::Bool))?;
                }
                Op::IntUn { a, .. } => {
                    let t = ty(*a)?;
                    if !t.is_int() {
                        return Err(format!("{what}: int op on {t:?}")); // alloc: Cold
                    }
                    need(Some(t))?;
                }
                Op::Icmp { a, b, .. } => {
                    let t = same(*a, *b, what)?;
                    if !t.is_int() && t != Type::Ptr && t != Type::Bool {
                        return Err(format!("{what}: icmp on {t:?}")); // alloc: Cold
                    }
                    need(Some(Type::Bool))?;
                }
                Op::FloatBin { a, b, .. } => {
                    let t = same(*a, *b, what)?;
                    if !t.is_float() {
                        return Err(format!("{what}: float op on {t:?}")); // alloc: Cold
                    }
                    need(Some(t))?;
                }
                Op::FloatUn { a, .. } => {
                    let t = ty(*a)?;
                    if !t.is_float() {
                        return Err(format!("{what}: float op on {t:?}")); // alloc: Cold
                    }
                    need(Some(t))?;
                }
                Op::Fcmp { a, b, .. } => {
                    let t = same(*a, *b, what)?;
                    if !t.is_float() {
                        return Err(format!("{what}: fcmp on {t:?}")); // alloc: Cold
                    }
                    need(Some(Type::Bool))?;
                }
                Op::Conv { op, to, a } => {
                    let from = ty(*a)?;
                    let ok = match op {
                        ConvOp::Sext | ConvOp::Zext => to.is_int() && (from == Type::Bool || (from.is_int() && to.bytes() > from.bytes())),
                        ConvOp::Trunc => from.is_int() && to.is_int() && to.bytes() < from.bytes(),
                        ConvOp::SintToFloat | ConvOp::UintToFloat => from.is_int() && to.is_float(),
                        ConvOp::FloatToSint | ConvOp::FloatToUint => from.is_float() && to.is_int(),
                        ConvOp::Fpromote => from == Type::F32 && *to == Type::F64,
                        ConvOp::Fdemote => from == Type::F64 && *to == Type::F32,
                        ConvOp::Bitcast => from.bytes() == to.bytes() && from != *to && from != Type::Ptr && *to != Type::Ptr && from.is_vector() == to.is_vector(),
                        ConvOp::PtrToInt => from == Type::Ptr && *to == Type::I64,
                        ConvOp::IntToPtr => from == Type::I64 && *to == Type::Ptr,
                    };
                    if !ok {
                        return Err(format!("{what}: invalid {op:?} from {from:?} to {to:?}")); // alloc: Cold
                    }
                    need(Some(*to))?;
                }
                Op::Select { cond, a, b } => {
                    expect(*cond, Type::Bool, what)?;
                    let t = same(*a, *b, what)?;
                    if t.is_vector() {
                        return Err(format!("{what}: select on {t:?} (use vselect)")); // alloc: Cold
                    }
                    need(Some(t))?;
                }
                Op::Load { ty: t, ptr, .. } => {
                    if t.is_vector() {
                        return Err(format!("{what}: load of {t:?} (use vload)")); // alloc: Cold
                    }
                    expect(*ptr, Type::Ptr, what)?;
                    need(Some(*t))?;
                }
                Op::LoadZext { ty: t, ptr, .. } => {
                    if !matches!(t, Type::I8 | Type::I16 | Type::I32) {
                        return Err(format!("{what}: load_zext needs a narrow integer type")); // alloc: Cold
                    }
                    expect(*ptr, Type::Ptr, what)?;
                    need(Some(Type::I64))?;
                }
                Op::Store { ptr, value, .. } => {
                    expect(*ptr, Type::Ptr, what)?;
                    if ty(*value)?.is_vector() {
                        return Err(format!("{what}: store of a vector (use vstore)")); // alloc: Cold
                    }
                    need(None)?;
                }
                Op::Prefetch { ptr, .. } => {
                    expect(*ptr, Type::Ptr, what)?;
                    need(None)?;
                }
                Op::Crc32c { acc, data } => {
                    expect(*acc, Type::I64, what)?;
                    expect(*data, Type::I64, what)?;
                    need(Some(Type::I64))?;
                }
                Op::AddHi { alo, blo, ahi, bhi } => {
                    for x in [alo, blo, ahi, bhi] {
                        expect(*x, Type::I64, what)?;
                    }
                    need(Some(Type::I64))?;
                }
                Op::StackAddr { slot } => {
                    if slot.index() >= f.stack_slots.len() {
                        return Err(format!("{what}: stack slot out of range")); // alloc: Cold
                    }
                    need(Some(Type::Ptr))?;
                }
                Op::PtrAdd { base, index, .. } => {
                    expect(*base, Type::Ptr, what)?;
                    expect(*index, Type::I64, what)?;
                    need(Some(Type::Ptr))?;
                }
                Op::Call { callee, args } => {
                    let c = p.funcs.get(callee.index()).ok_or_else(|| format!("{what}: callee out of range"))?; // alloc: Cold
                    check_args(what, args, &c.params, &ty)?;
                    let ret = match c.returns.as_slice() {
                        [] => None,
                        [t] => Some(*t),
                        _ => return Err(format!("{what}: multi-value call results not supported in Op::Call")), // alloc: Cold
                    };
                    need(ret)?;
                }
                Op::CallHost { callee, args } => {
                    let c = p.host_fns.get(callee.index()).ok_or_else(|| format!("{what}: host fn out of range"))?; // alloc: Cold
                    check_args(what, args, &c.params, &ty)?;
                    need(c.ret)?;
                }
                Op::VecLoad { ty: t, ptr, .. } => {
                    if !t.is_vector() {
                        return Err(format!("{what}: vload of {t:?}")); // alloc: Cold
                    }
                    expect(*ptr, Type::Ptr, what)?;
                    need(Some(*t))?;
                }
                Op::VecStore { ptr, value, .. } => {
                    expect(*ptr, Type::Ptr, what)?;
                    if !ty(*value)?.is_vector() {
                        return Err(format!("{what}: vstore of a scalar")); // alloc: Cold
                    }
                    need(None)?;
                }
                Op::VecBin { op, a, b } => {
                    let t = same(*a, *b, what)?;
                    let ok = match op {
                        VecBinOp::And | VecBinOp::Or | VecBinOp::Xor => t.is_vector(),
                        VecBinOp::Mul => t.is_vector() && t != Type::V2I64,
                        VecBinOp::Add | VecBinOp::Sub => t.is_vector(),
                        VecBinOp::Smin | VecBinOp::Smax | VecBinOp::Umin | VecBinOp::Umax => t.is_int_vector(),
                    };
                    if !ok {
                        return Err(format!("{what}: {op:?} on {t:?}")); // alloc: Cold
                    }
                    need(Some(t))?;
                }
                Op::VecShift { a, count, .. } => {
                    let t = ty(*a)?;
                    if !t.is_int_vector() {
                        return Err(format!("{what}: vector shift on {t:?}")); // alloc: Cold
                    }
                    if u32::from(*count) >= t.lane().bytes() * 8 {
                        return Err(format!("{what}: shift count {count} exceeds the lane width")); // alloc: Cold
                    }
                    need(Some(t))?;
                }
                Op::VecCmp { a, b, .. } => {
                    let t = same(*a, *b, what)?;
                    if !t.is_int_vector() {
                        return Err(format!("{what}: vcmp on {t:?}")); // alloc: Cold
                    }
                    need(Some(t))?;
                }
                Op::VecSelect { mask, a, b } => {
                    let t = same(*a, *b, what)?;
                    let tm = same(*mask, *a, what)?;
                    if !tm.is_vector() {
                        return Err(format!("{what}: vselect on {t:?}")); // alloc: Cold
                    }
                    need(Some(t))?;
                }
                Op::VecReduceAdd { a } => {
                    let t = ty(*a)?;
                    let r = match t {
                        _ if t.is_int_vector() => Type::I64,
                        Type::V2F64 => Type::F64,
                        Type::V4F32 => Type::F32,
                        _ => return Err(format!("{what}: vreduce_add on {t:?}")), // alloc: Cold
                    };
                    need(Some(r))?;
                }
                Op::VecMovmask { a } => {
                    if !ty(*a)?.is_vector() {
                        return Err(format!("{what}: vmovmask on a scalar")); // alloc: Cold
                    }
                    need(Some(Type::I64))?;
                }
                Op::VecSplat { ty: t, a } => {
                    if !t.is_vector() {
                        return Err(format!("{what}: vsplat to {t:?}")); // alloc: Cold
                    }
                    expect(*a, t.lane(), what)?;
                    need(Some(*t))?;
                }
                Op::VecExtract { a, lane } => {
                    let t = ty(*a)?;
                    if !t.is_vector() || u32::from(*lane) >= t.lanes() {
                        return Err(format!("{what}: vextract lane {lane} of {t:?}")); // alloc: Cold
                    }
                    need(Some(t.lane()))?;
                }
                Op::VecInsert { a, lane, value } => {
                    let t = ty(*a)?;
                    if !t.is_vector() || u32::from(*lane) >= t.lanes() {
                        return Err(format!("{what}: vinsert lane {lane} of {t:?}")); // alloc: Cold
                    }
                    expect(*value, t.lane(), what)?;
                    need(Some(t))?;
                }
                Op::VecWiden { a, .. } => {
                    let t = ty(*a)?;
                    let r = t.widened().ok_or_else(|| format!("{what}: vwiden of {t:?}"))?; // alloc: Cold
                    need(Some(r))?;
                }
                Op::VecNarrow { a, b } => {
                    let t = same(*a, *b, what)?;
                    let r = t.narrowed().ok_or_else(|| format!("{what}: vnarrow of {t:?}"))?; // alloc: Cold
                    need(Some(r))?;
                }
            }
        }
        let what = Site::Term(bi);
        match &b.term {
            Terminator::Br(e) => check_edge(e, what)?,
            Terminator::CondBr { cond, then_edge, else_edge } => {
                expect(*cond, Type::Bool, what)?;
                check_edge(then_edge, what)?;
                check_edge(else_edge, what)?;
            }
            Terminator::Switch { value, cases, default } => {
                let t = ty(*value)?;
                if !t.is_int() {
                    return Err(format!("{what}: switch on {t:?}")); // alloc: Cold
                }
                for (i, c) in cases.iter().enumerate() {
                    if cases[..i].iter().any(|d| d.value == c.value) {
                        return Err(format!("{what}: duplicate case {}", c.value)); // alloc: Cold
                    }
                    check_edge(&c.edge, what)?;
                }
                check_edge(default, what)?;
            }
            Terminator::Return(vals) => {
                if vals.len() != f.returns.len() {
                    return Err(format!("{what}: returns {} values, function declares {}", vals.len(), f.returns.len())); // alloc: Cold
                }
                for (&v, &t) in vals.iter().zip(&f.returns) {
                    expect(v, t, what)?;
                }
            }
            Terminator::Unreachable => {}
        }
    }
    Ok(())
}

fn check_args(what: Site, args: &[Value], params: &[Type], ty: &dyn Fn(Value) -> Result<Type, String>) -> Result<(), String> { // alloc: Cold
    if args.len() != params.len() {
        return Err(format!("{what}: {} args, callee has {} params", args.len(), params.len())); // alloc: Cold
    }
    for (&a, &t) in args.iter().zip(params) {
        let got = ty(a)?;
        if got != t {
            return Err(format!("{what}: arg v{} is {got:?}, expected {t:?}", a.0)); // alloc: Cold
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// Register state: the carried-cell law
// ----------------------------------------------------------------------------

use crate::promote::{op_values_mut, term_values_mut, Cell, Report};

/// What the generated code holds as the cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Content {
    /// Never read or written on this path: memory's own value.
    Initial,
    Val(Value),
    /// A host may have changed the cell and the code has not reloaded it.
    Stale,
    /// Different on different paths into the block (an error only when
    /// used: read, written back, or passed on an edge).
    Mixed,
}

#[derive(Clone, Copy, Debug)]
enum Event {
    /// The f1 instruction at this index.
    Inst(usize),
    /// A deleted store: the content becomes the (mapped) value.
    Ghost { cell: usize, value: Value },
    /// A deleted load: its f0 result, whose stand-in must be the content.
    Read { cell: usize, result: Value },
}

/// Prove the register-state law (`docs/design/gravity-simple/register-state.md`
/// §1.2) for `f1` = `promote(f0, cells)`: every deleted load reads the
/// cell's current content, every appended edge argument is the
/// predecessor's content, and at every `Return`, every host call whose
/// mask names the cell and every `Call`, memory holds the content.
/// `report` supplies which cells were promoted and which appended
/// parameter stands for which cell; both are checked here.
pub fn verify_carried(m: &Mint, f0: &Function, f1: &Function, cells: &[Cell], report: &Report, host_state: &[StateMask]) -> Result<(), String> { // alloc: Cold
    let n0 = f0.values.len();
    let nb0 = f0.blocks.len();
    if report.promoted == 0 {
        // Nothing carried: the pass left the function alone.
        return if f0 == f1 { Ok(()) } else { Err(String::from("no cell promoted, yet the function changed")) }; // alloc: Cold
    }
    if f1.blocks.len() < nb0 {
        return Err(format!("blocks removed: {} -> {}", nb0, f1.blocks.len())); // alloc: Cold
    }
    // Blocks the pass appended: each splits one f0 edge (no parameters,
    // only inserted stores and root loads, a `Br` to an f0 block).
    for (bi, b) in f1.blocks.iter().enumerate().skip(nb0) {
        if !b.params.is_empty() {
            return Err(format!("b{bi}: an appended block with parameters")); // alloc: Cold
        }
        match &b.term {
            Terminator::Br(e) if e.target.index() < nb0 => {}
            _ => return Err(format!("b{bi}: an appended block must branch to an original block")), // alloc: Cold
        }
    }
    if f0.entry != f1.entry {
        return Err(String::from("entry changed")); // alloc: Cold
    }
    let promoted = |ci: usize| ci < 64 && report.promoted >> ci & 1 != 0;
    // Roots in f0 (the pass may have deleted a root's own load in f1).
    let mut root_bits0 = m.vec_filled(0u64, n0);
    for (ci, c) in cells.iter().enumerate() {
        if !promoted(ci) {
            continue;
        }
        match c.root {
            crate::promote::Root::Value(r) => root_bits0[r.index()] |= 1 << ci,
            crate::promote::Root::LoadedFrom { ptr, offset } => {
                for b in &f0.blocks {
                    for i in &b.insts {
                        if let (Some(r), Op::Load { ty: Type::Ptr, ptr: p, offset: o, .. }) = (i.result, &i.op) {
                            if *p == ptr && *o == offset {
                                root_bits0[r.index()] |= 1 << ci;
                            }
                        }
                    }
                }
            }
        }
    }
    let cell_of0 = |ptr: Value, offset: i32| -> Option<usize> {
        let bits = root_bits0.get(ptr.index()).copied().unwrap_or(0);
        (0..cells.len()).find(|&ci| bits & (1 << ci) != 0 && cells[ci].offset == offset)
    };
    // Roots in f1: the pass inserts fresh root loads, so resolve by definition.
    let mut root_bits1 = m.vec_filled(0u64, f1.values.len());
    for (ci, c) in cells.iter().enumerate() {
        if !promoted(ci) {
            continue;
        }
        match c.root {
            crate::promote::Root::Value(r) => root_bits1[r.index()] |= 1 << ci,
            crate::promote::Root::LoadedFrom { ptr, offset } => {
                for b in &f1.blocks {
                    for i in &b.insts {
                        if let (Some(r), Op::Load { ty: Type::Ptr, ptr: p, offset: o, .. }) = (i.result, &i.op) {
                            if *p == ptr && *o == offset {
                                root_bits1[r.index()] |= 1 << ci;
                            }
                        }
                    }
                }
            }
        }
    }
    let cell_of1 = |ptr: Value, offset: i32| -> Option<usize> {
        let bits = root_bits1.get(ptr.index()).copied().unwrap_or(0);
        (0..cells.len()).find(|&ci| bits & (1 << ci) != 0 && cells[ci].offset == offset)
    };
    let access0 = |op: &Op| -> Option<(usize, Option<Value>)> {
        match op {
            Op::Load { ptr, offset, .. } | Op::LoadZext { ptr, offset, .. } => cell_of0(*ptr, *offset).map(|c| (c, None)),
            Op::Store { ptr, offset, value, .. } => cell_of0(*ptr, *offset).map(|c| (c, Some(*value))),
            _ => None,
        }
    };
    // Which f0 results are deleted loads.
    let mut deleted_load = m.vec_filled(false, n0);
    for b in &f0.blocks {
        for i in &b.insts {
            if let (Some(r), Some((_, None))) = (i.result, access0(&i.op)) {
                deleted_load[r.index()] = true;
            }
        }
    }
    // The appended parameters, by block: (value, cell).
    let mut appended: IrVec<IrVec<(Value, usize)>> = m.collect((0..f1.blocks.len()).map(|_| m.vec()));
    // An f1 edge into an appended block stands for the edge that block
    // branches to; the arguments live on the appended block's `Br`.
    let through = |e: &Edge| -> Edge {
        if e.target.index() >= nb0 {
            if let Terminator::Br(inner) = &f1.blocks[e.target.index()].term {
                return inner.clone_in(m);
            }
        }
        e.clone_in(m)
    };
    for &(b, v, c) in &report.params {
        if !promoted(c) {
            return Err(format!("b{}: parameter for an unpromoted cell {c}", b.0)); // alloc: Cold
        }
        appended[b.index()].push((v, c));
    }
    for (bi, (b0, b1)) in f0.blocks.iter().zip(&f1.blocks).enumerate() {
        let n = b0.params.len();
        if b1.params.len() < n || b1.params[..n] != b0.params[..] {
            return Err(format!("b{bi}: original parameters changed")); // alloc: Cold
        }
        let app = m.collect(appended[bi].iter().map(|&(v, _)| v));
        if b1.params[n..] != app[..] {
            return Err(format!("b{bi}: appended parameters {:?} are not the reported {:?}", &b1.params[n..], app)); // alloc: Cold
        }
    }
    // ---- structural diff -> events per block, the value map ----
    let mut map = m.collect((0..n0).map(|i| if deleted_load[i] { None } else { Some(Value(i as u32)) }));
    let mut events: IrVec<IrVec<Event>> = m.vec_with_capacity(f0.blocks.len());
    // Deferred operand bindings: (f0 value, f1 value) learned at a match.
    // `what` names the place for an error only: formatted when the binding fails, never on the way through
    let bind = |a0: Value, a1: Value, map: &mut IrVec<Option<Value>>, what: &dyn Fn() -> String| -> Result<(), String> { // alloc: Cold
        match map[a0.index()] {
            Some(m) if m == a1 => Ok(()),
            Some(m) => Err(format!("{}: v{} stands for v{} here but v{} elsewhere", what(), a1.0, a0.0, m.0)), // alloc: Cold
            None => {
                map[a0.index()] = Some(a1);
                Ok(())
            }
        }
    };
    // Values f1 still reads (a dead pure f0 instruction may be gone).
    let mut used1 = m.vec_filled(false, f1.values.len());
    let (mut vs0, mut vs1): (IrVec<Value>, IrVec<Value>) = (m.vec(), m.vec());
    for b in f1.blocks.iter() {
        let mut mark = |v: Value| used1[v.index()] = true;
        b.insts.iter().for_each(|i| i.op.for_each_operand(&mut mark));
        b.term.for_each_operand(&mut mark);
        b.term.edges().for_each(|e| e.args.iter().for_each(|&v| mark(v)));
    }
    let empty = BasicBlock { params: m.slice_from(&[]), insts: m.vec(), term: Terminator::Unreachable };
    // f0's unreachable blocks are emptied by the pass: diffed as empty.
    let mut live0 = m.vec_filled(false, nb0);
    for b in crate::promote::rpo(f0, m) {
        live0[b.index()] = true;
    }
    for (bi, b1) in f1.blocks.iter().enumerate() {
        let b0 = match f0.blocks.get(bi) {
            Some(b) if live0[bi] => b,
            Some(b) => {
                if !b1.insts.is_empty() || b1.term != Terminator::Unreachable || b1.params != b.params {
                    return Err(format!("b{bi}: an unreachable block was not emptied")); // alloc: Cold
                }
                &empty
            }
            None => &empty,
        };
        let mut ev = m.vec_with_capacity(b1.insts.len() + 4);
        let (mut i0, mut i1) = (0usize, 0usize);
        loop {
            // An inserted f1 instruction: a fresh value, or a store to a
            // promoted cell. A reload (after a host) or a conv precedes the
            // deleted accesses that follow it; a write-back (before a host
            // or a return) follows the deleted accesses before it.
            let inserted = b1.insts.get(i1).map(|inst1| match (inst1.result, &inst1.op) {
                (Some(r), _) if r.index() >= n0 => true,
                (None, Op::Store { ptr, offset, .. }) => cell_of1(*ptr, *offset).is_some_and(promoted),
                _ => false,
            });
            let deleted0 = b0.insts.get(i0).and_then(|inst0| access0(&inst0.op)).is_some_and(|(c, _)| promoted(c));
            if inserted == Some(true) {
                let inst1 = &b1.insts[i1];
                let is_store = matches!(inst1.op, Op::Store { .. });
                if !(is_store && deleted0) {
                    match &inst1.op {
                        Op::Load { .. } | Op::Store { .. } | Op::Conv { op: ConvOp::Zext, .. } => {}
                        other => return Err(format!("b{bi}: inserted instruction of an unexpected kind: {other:?}")), // alloc: Cold
                    }
                    ev.push(Event::Inst(i1));
                    i1 += 1;
                    continue;
                }
            }
            if i0 >= b0.insts.len() {
                if i1 < b1.insts.len() {
                    return Err(format!("b{bi}: extra instruction {:?}", b1.insts[i1])); // alloc: Cold
                }
                break;
            }
            let inst0 = &b0.insts[i0];
            if let Some((cell, stored)) = access0(&inst0.op) {
                if promoted(cell) {
                    // Deleted in f1.
                    match stored {
                        Some(v) => {
                            if map[v.index()].is_none() {
                                return Err(format!("b{bi}: store of v{} whose stand-in is unknown", v.0)); // alloc: Cold
                            }
                            ev.push(Event::Ghost { cell, value: v });
                        }
                        None => ev.push(Event::Read { cell, result: inst0.result.expect("load result") }),
                    }
                    i0 += 1;
                    continue;
                }
            }
            // A pure instruction nothing reads any more (its user was a
            // deleted access) may be gone.
            if let Some(r) = inst0.result {
                let gone = crate::promote::pure(&inst0.op) && !used1[r.index()] && b1.insts.get(i1).is_none_or(|i| i.result != Some(r));
                if gone {
                    i0 += 1;
                    continue;
                }
            }
            // Must match f1[i1].
            let Some(inst1) = b1.insts.get(i1) else { return Err(format!("b{bi}: missing instruction {:?}", inst0)) }; // alloc: Cold
            if inst0.result != inst1.result {
                return Err(format!("b{bi}: instruction {:?} became {:?}", inst0, inst1)); // alloc: Cold
            }
            // (`operands_into` visits the operands in `op_values_mut`'s order)
            (vs0.clear(), vs1.clear());
            inst0.op.for_each_operand(|v| vs0.push(v));
            inst1.op.for_each_operand(|v| vs1.push(v));
            if vs0.len() != vs1.len() {
                return Err(format!("b{bi}: instruction {:?} became {:?}", inst0, inst1)); // alloc: Cold
            }
            for (&a0, &a1) in vs0.iter().zip(&vs1) {
                bind(a0, a1, &mut map, &|| format!("b{bi}: {:?}", inst0))?; // alloc: Cold
            }
            // The op with f1's operands must be f1's op: the op itself where the operands are the same (no copy)
            let same = if vs0 == vs1 {
                inst0.op == inst1.op
            } else {
                let mut o0 = inst0.op.clone_in(m);
                let mut k = 0;
                op_values_mut(&mut o0, &mut |v: &mut Value| {
                    *v = vs1[k];
                    k += 1;
                });
                o0 == inst1.op
            };
            if !same {
                return Err(format!("b{bi}: instruction {:?} became {:?}", inst0, inst1)); // alloc: Cold
            }
            ev.push(Event::Inst(i1));
            i0 += 1;
            i1 += 1;
        }
        if bi >= nb0 || !live0[bi] {
            // An appended block: its `Br` is checked with the edge it
            // splits. An emptied block has no terminator to check.
            events.push(ev);
            continue;
        }
        // Terminators.
        let mut t0 = b0.term.clone_in(m);
        let mut vs0: IrVec<Value> = m.vec();
        term_values_mut(&mut t0, &mut |v: &mut Value| vs0.push(*v));
        // f1's edges carry appended arguments (and may pass through an
        // appended block): strip them for the comparison.
        let mut t1 = b1.term.clone_in(m);
        for (ei, e) in b0.term.edges().enumerate() {
            let e1 = crate::promote::edge_mut(&mut t1, ei);
            *e1 = through(e1);
            let want = e.args.len() + appended[e.target.index()].len();
            if e1.args.len() != want {
                return Err(format!("b{bi}: edge {ei} to b{} carries {} arguments, expected {want}", e.target.0, e1.args.len())); // alloc: Cold
            }
            e1.args = m.slice_from(&e1.args[..e.args.len()]);
        }
        let mut vs1: IrVec<Value> = m.vec();
        term_values_mut(&mut t1, &mut |v: &mut Value| vs1.push(*v));
        if vs0.len() != vs1.len() {
            return Err(format!("b{bi}: terminator changed")); // alloc: Cold
        }
        for (&a0, &a1) in vs0.iter().zip(&vs1) {
            bind(a0, a1, &mut map, &|| format!("b{bi}: terminator"))?; // alloc: Cold
        }
        let mut k = 0;
        term_values_mut(&mut t0, &mut |v: &mut Value| {
            *v = vs1[k];
            k += 1;
        });
        if t0 != t1 {
            return Err(format!("b{bi}: terminator changed")); // alloc: Cold
        }
        events.push(ev);
    }
    // ---- content dataflow on f1 ----
    // Per cell: `content` (what the code holds) and `dirty` (the content
    // differs from memory: a deleted store since the last write-back).
    let nb = f1.blocks.len();
    let nc = cells.len();
    let mut preds: IrVec<IrVec<(Block, usize)>> = m.collect((0..nb).map(|_| m.vec()));
    for (bi, b) in f1.blocks.iter().enumerate() {
        for (ei, e) in b.term.edges().enumerate() {
            preds[e.target.index()].push((Block(bi as u32), ei));
        }
    }
    let order = crate::promote::rpo(f1, m);
    let mut done = m.vec_filled(false, nb);
    let mut out_dirty: IrVec<IrVec<bool>> = m.collect((0..nb).map(|_| m.vec_filled(false, nc)));
    let mut out_content: IrVec<IrVec<Content>> = m.collect((0..nb).map(|_| m.vec_filled(Content::Initial, nc)));
    let call_mask = |op: &Op| -> Option<StateMask> {
        match op {
            Op::CallHost { callee, .. } => Some(host_state[callee.index()]).filter(|m| *m != StateMask::NONE),
            Op::Call { .. } => Some(StateMask::ALL),
            _ => None,
        }
    };
    for _round in 0..(nb + 2) {
        let mut changed = false;
        for &b in &order {
            let bi = b.index();
            let mut dirty = m.vec_filled(false, nc);
            let mut content = m.vec_filled(Content::Initial, nc);
            let live = m.collect(preds[bi].iter().filter(|(p, _)| done[p.index()]));
            for ci in 0..nc {
                if !promoted(ci) {
                    continue;
                }
                dirty[ci] = live.iter().any(|&&(p, _)| out_dirty[p.index()][ci]);
                if let Some(&(pv, _)) = appended[bi].iter().find(|&&(_, c)| c == ci) {
                    let n = f0.blocks[bi].params.len();
                    let k = f1.blocks[bi].params[n..].iter().position(|&v| v == pv).expect("appended param");
                    for &&(p, ei) in &live {
                        let e = f1.blocks[p.index()].term.edges().nth(ei).expect("edge");
                        // An appended block's `Br` carries the original edge's
                        // arguments; the split edge in the original is the
                        // one into the appended block.
                        let n_orig = if p.index() >= nb0 {
                            let (q, qi) = split_origin(f1, nb0, p);
                            f0.blocks[q.index()].term.edges().nth(qi).expect("edge").args.len()
                        } else {
                            f0.blocks[p.index()].term.edges().nth(ei).expect("edge").args.len()
                        };
                        let arg = e.args[n_orig + k];
                        match out_content[p.index()][ci] {
                            Content::Val(v) if v == arg => {}
                            other => return Err(format!("b{bi}: cell {ci}: edge from b{} passes v{} but the predecessor's content is {other:?}", p.0, arg.0)), // alloc: Cold
                        }
                    }
                    content[ci] = Content::Val(pv);
                } else {
                    let mut c: Option<Content> = None;
                    for &&(p, _) in &live {
                        let pc = out_content[p.index()][ci];
                        c = Some(match c {
                            None => pc,
                            Some(x) if x == pc => x,
                            Some(_) => Content::Mixed,
                        });
                    }
                    content[ci] = c.unwrap_or(Content::Initial);
                }
            }
            for ev in &events[bi] {
                match ev {
                    Event::Ghost { cell, value } => {
                        let v = map[value.index()].expect("checked");
                        content[*cell] = Content::Val(v);
                        dirty[*cell] = true;
                    }
                    Event::Read { cell, result } => {
                        if let Some(s) = map[result.index()] {
                            // A narrow cell's zext stand-in: the conv's operand is the content.
                            let s = f1.blocks[bi].insts.iter().find_map(|i| match (i.result, &i.op) {
                                (Some(r), Op::Conv { op: ConvOp::Zext, a, .. }) if r == s && r.index() >= n0 => Some(*a),
                                _ => None,
                            }).unwrap_or(s);
                            match content[*cell] {
                                Content::Val(v) if v == s => {}
                                other => return Err(format!("b{bi}: cell {}: a read stands in v{} but the content is {other:?}", cell, s.0)), // alloc: Cold
                            }
                        }
                    }
                    Event::Inst(ii) => {
                        let inst = &f1.blocks[bi].insts[*ii];
                        match &inst.op {
                            Op::Load { ptr, offset, .. } if cell_of1(*ptr, *offset).is_some_and(promoted) => {
                                let c = cell_of1(*ptr, *offset).unwrap();
                                content[c] = Content::Val(inst.result.unwrap());
                                dirty[c] = false;
                            }
                            Op::Store { ptr, offset, value, .. } if cell_of1(*ptr, *offset).is_some_and(promoted) => {
                                let c = cell_of1(*ptr, *offset).unwrap();
                                if content[c] != Content::Val(*value) {
                                    return Err(format!("b{bi}: cell {c}: write-back of v{} but the content is {:?}", value.0, content[c])); // alloc: Cold
                                }
                                dirty[c] = false;
                            }
                            op => {
                                if let Some(m) = call_mask(op) {
                                    for ci in 0..nc {
                                        if !promoted(ci) || !m.contains(cells[ci].class) {
                                            continue;
                                        }
                                        if dirty[ci] {
                                            return Err(format!("b{bi}: cell {ci}: host call {op:?} sees memory behind the content {:?}", content[ci])); // alloc: Cold
                                        }
                                        content[ci] = Content::Stale;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let Terminator::Return(_) = f1.blocks[bi].term {
                for ci in 0..nc {
                    if promoted(ci) && dirty[ci] {
                        return Err(format!("b{bi}: cell {ci}: returns with memory behind the content {:?}", content[ci])); // alloc: Cold
                    }
                }
            }
            if out_dirty[bi] != dirty || out_content[bi] != content || !done[bi] {
                changed = true;
            }
            out_dirty[bi] = dirty;
            out_content[bi] = content;
            done[bi] = true;
        }
        if !changed {
            return Ok(());
        }
    }
    Err(String::from("the content dataflow did not converge")) // alloc: Cold
}

/// The original edge `(block, edge index)` an appended block splits.
fn split_origin(f1: &Function, nb0: usize, b: Block) -> (Block, usize) {
    for (pi, p) in f1.blocks.iter().enumerate().take(nb0) {
        for (ei, e) in p.term.edges().enumerate() {
            if e.target == b {
                return (Block(pi as u32), ei);
            }
        }
    }
    unreachable!("an appended block with no predecessor")
}
