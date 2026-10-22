//! S-expression text form. The printer emits it (dumps, verify findings); the loader reads it back, test input only:
//! built under `cfg(test)` or the `text-loader` feature, which the dev-dependencies of the crates whose tests load text
//! turn on.
//!
//! ```text
//! (program
//!   (host add_err (i64 i64) i64 may_error touches_memory)
//!   (fn sum ((ctx ptr) (n i64)) (i64)
//!     (slots (16 8))
//!     (block entry ((ctx ptr) (n i64))
//!       (acc0 (const i64 0))
//!       (br loop acc0 n))
//!     (block loop ((acc i64) (i i64))
//!       (done (icmp sle i zero))
//!       (condbr done (exit acc) (body acc i)))
//!     ...))
//! ```
//!
//! Instruction forms (result name first, then the op form):
//!   (const TY BITS)              BITS: integer, or float literal for f32/f64,
//!                                or `true`/`false` for bool
//!   (iadd a b) ... (sshr a b)    IntBinOp by lowercase name with `i` prefix
//!   (sadd_ovf a b) (ssub_ovf a b) (smul_ovf a b)
//!   (ineg a) (inot a) (clz a) (ctz a) (popcnt a) (bswap a)
//!   (icmp COND a b)              COND: eq ne slt sle sgt sge ult ule ugt uge
//!   (fadd a b) (fsub a b) (fmul a b) (fdiv a b) (fmin a b) (fmax a b) (copysign a b)
//!   (fneg a) (fabs a) (sqrt a) (ceil a) (floor a) (trunc a) (nearest a)
//!   (fcmp COND a b)              COND: eq ne lt le gt ge ord uno
//!   (sext TY a) (zext TY a) (itrunc TY a) (sitofp TY a) (uitofp TY a)
//!   (fptosi TY a) (fptoui TY a) (fpromote a) (fdemote a) (bitcast TY a)
//!   (ptrtoint a) (inttoptr a)
//!   (select c a b)
//!   (load TY p OFF [unaligned])  (store p OFF v [unaligned])
//!   (stack_addr N)  (ptr_add base index SCALE OFF)
//!   (prefetch p OFF)  (crc32c acc data)  (addhi alo blo ahi bhi)
//!   (call FN args...)  (call_host HOST args...)
//! Vectors (types v16i8 v8i16 v4i32 v2i64 v4f32 v2f64; simd.md):
//!   (vload TY p OFF [unaligned])  ((vstore p OFF v [unaligned]))
//!   (vadd a b) (vsub a b) (vmul a b) (vand a b) (vor a b) (vxor a b)
//!   (vshl a N) (vushr a N) (vsshr a N)   N: immediate lane shift
//!   (vcmp COND a b)              COND as icmp; a lane mask vector
//!   (vselect m a b) (vreduce_add a) (vmovmask a) (vsplat TY a)
//!   (vextract a N) (vinsert a N v)
//!   (vwiden s|u lo|hi a)         sign/zero extend the low/high lanes
//!   (vnarrow a b)                a's lanes truncated low, b's high
//! Void instructions (store, void calls) are written without a result name:
//!   ((store p 0 v))  or  ((call_host h a))
//! Terminators:
//!   (br BLOCK args...)  (condbr c (BLOCK args...) (BLOCK args...))
//!   (switch v (case N (BLOCK args...))... (default (BLOCK args...)))
//!   (return args...)  (unreachable)

#[cfg(any(test, feature = "text-loader"))]
use crate::builder::*;
use crate::*;
#[cfg(any(test, feature = "text-loader"))]
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString}; // alloc: Cold
#[cfg(any(test, feature = "text-loader"))]
use alloc::vec::Vec;
use core::fmt::Write;

// ----------------------------------------------------------------------------
// Printer
// ----------------------------------------------------------------------------

fn ty_name(t: Type) -> &'static str {
    match t {
        Type::Bool => "bool",
        Type::I8 => "i8",
        Type::I16 => "i16",
        Type::I32 => "i32",
        Type::I64 => "i64",
        Type::F32 => "f32",
        Type::F64 => "f64",
        Type::Ptr => "ptr",
        Type::V16I8 => "v16i8",
        Type::V8I16 => "v8i16",
        Type::V4I32 => "v4i32",
        Type::V2I64 => "v2i64",
        Type::V4F32 => "v4f32",
        Type::V2F64 => "v2f64",
    }
}

fn vec_bin_name(op: VecBinOp) -> &'static str {
    match op {
        VecBinOp::Add => "vadd",
        VecBinOp::Sub => "vsub",
        VecBinOp::Mul => "vmul",
        VecBinOp::And => "vand",
        VecBinOp::Or => "vor",
        VecBinOp::Xor => "vxor",
        VecBinOp::Smin => "vsmin",
        VecBinOp::Smax => "vsmax",
        VecBinOp::Umin => "vumin",
        VecBinOp::Umax => "vumax",
    }
}
fn vec_shift_name(op: VecShiftOp) -> &'static str {
    match op {
        VecShiftOp::Shl => "vshl",
        VecShiftOp::Ushr => "vushr",
        VecShiftOp::Sshr => "vsshr",
    }
}

fn int_bin_name(op: IntBinOp) -> &'static str {
    match op {
        IntBinOp::Add => "iadd",
        IntBinOp::Sub => "isub",
        IntBinOp::Mul => "imul",
        IntBinOp::Sdiv => "sdiv",
        IntBinOp::Udiv => "udiv",
        IntBinOp::Srem => "srem",
        IntBinOp::Urem => "urem",
        IntBinOp::And => "iand",
        IntBinOp::Or => "ior",
        IntBinOp::Xor => "ixor",
        IntBinOp::Shl => "shl",
        IntBinOp::Ushr => "ushr",
        IntBinOp::Sshr => "sshr",
        IntBinOp::Smin => "smin",
        IntBinOp::Smax => "smax",
        IntBinOp::Umin => "umin",
        IntBinOp::Umax => "umax",
        IntBinOp::Smulh => "smulh",
        IntBinOp::Umulh => "umulh",
        IntBinOp::Rotl => "rotl",
    }
}
fn int_ovf_name(op: IntOvfOp) -> &'static str {
    match op {
        IntOvfOp::SaddOvf => "sadd_ovf",
        IntOvfOp::SsubOvf => "ssub_ovf",
        IntOvfOp::SmulOvf => "smul_ovf",
    }
}
fn int_un_name(op: IntUnOp) -> &'static str {
    match op {
        IntUnOp::Neg => "ineg",
        IntUnOp::Not => "inot",
        IntUnOp::Clz => "clz",
        IntUnOp::Ctz => "ctz",
        IntUnOp::Popcnt => "popcnt",
        IntUnOp::Bswap => "bswap",
        IntUnOp::Bitrev => "bitrev",
    }
}
fn int_cond_name(c: IntCond) -> &'static str {
    match c {
        IntCond::Eq => "eq",
        IntCond::Ne => "ne",
        IntCond::Slt => "slt",
        IntCond::Sle => "sle",
        IntCond::Sgt => "sgt",
        IntCond::Sge => "sge",
        IntCond::Ult => "ult",
        IntCond::Ule => "ule",
        IntCond::Ugt => "ugt",
        IntCond::Uge => "uge",
    }
}
fn float_bin_name(op: FloatBinOp) -> &'static str {
    match op {
        FloatBinOp::Add => "fadd",
        FloatBinOp::Sub => "fsub",
        FloatBinOp::Mul => "fmul",
        FloatBinOp::Div => "fdiv",
        FloatBinOp::Min => "fmin",
        FloatBinOp::Max => "fmax",
        FloatBinOp::Copysign => "copysign",
    }
}
fn float_un_name(op: FloatUnOp) -> &'static str {
    match op {
        FloatUnOp::Neg => "fneg",
        FloatUnOp::Abs => "fabs",
        FloatUnOp::Sqrt => "sqrt",
        FloatUnOp::Ceil => "ceil",
        FloatUnOp::Floor => "floor",
        FloatUnOp::Trunc => "trunc",
        FloatUnOp::Nearest => "nearest",
    }
}
fn float_cond_name(c: FloatCond) -> &'static str {
    match c {
        FloatCond::Eq => "eq",
        FloatCond::Ne => "ne",
        FloatCond::Lt => "lt",
        FloatCond::Le => "le",
        FloatCond::Gt => "gt",
        FloatCond::Ge => "ge",
        FloatCond::Ordered => "ord",
        FloatCond::Unordered => "uno",
    }
}
fn conv_name(op: ConvOp) -> &'static str {
    match op {
        ConvOp::Sext => "sext",
        ConvOp::Zext => "zext",
        ConvOp::Trunc => "itrunc",
        ConvOp::SintToFloat => "sitofp",
        ConvOp::UintToFloat => "uitofp",
        ConvOp::FloatToSint => "fptosi",
        ConvOp::FloatToUint => "fptoui",
        ConvOp::Fpromote => "fpromote",
        ConvOp::Fdemote => "fdemote",
        ConvOp::Bitcast => "bitcast",
        ConvOp::PtrToInt => "ptrtoint",
        ConvOp::IntToPtr => "inttoptr",
    }
}

fn const_lit(ty: Type, bits: u64) -> String { // alloc: Cold
    match ty {
        Type::Bool => (if bits != 0 { "true" } else { "false" }).to_string(), // alloc: Cold
        Type::F32 => format!("{:?}", f32::from_bits(bits as u32)), // alloc: Cold
        Type::F64 => format!("{:?}", f64::from_bits(bits)), // alloc: Cold
        Type::I8 => (bits as u8 as i8).to_string(), // alloc: Cold
        Type::I16 => (bits as u16 as i16).to_string(), // alloc: Cold
        Type::I32 => (bits as u32 as i32).to_string(), // alloc: Cold
        Type::I64 | Type::Ptr => (bits as i64).to_string(), // alloc: Cold
        _ => unreachable!("no vector constants"),
    }
}

/// `items` written with `put`, space-separated.
fn join<'a, T: 'a>(out: &mut String, items: impl Iterator<Item = &'a T>, mut put: impl FnMut(&mut String, &'a T)) { // alloc: Cold
    for (i, x) in items.enumerate() {
        if i > 0 {
            out.push(' '); // alloc: Cold
        }
        put(out, x);
    }
}

fn edge(out: &mut String, e: &Edge) { // alloc: Cold
    let _ = write!(out, "(b{}", e.target.0); // alloc: Cold
    for a in e.args.iter() {
        let _ = write!(out, " v{}", a.0); // alloc: Cold
    }
    out.push(')'); // alloc: Cold
}

pub fn print_program(p: &Program) -> String { // alloc: Cold
    let mut out = String::from("(program\n"); // alloc: Cold
    for h in &p.host_fns {
        let _ = write!(out, "  (host {} (", h.name); // alloc: Cold
        join(&mut out, h.params.iter(), |o, &t| o.push_str(ty_name(t))); // alloc: Cold
        out.push(')'); // alloc: Cold
        match h.ret {
            Some(t) => {
                let _ = write!(out, " {}", ty_name(t)); // alloc: Cold
            }
            None => out.push_str(" void"), // alloc: Cold
        }
        if h.flags.may_error {
            out.push_str(" may_error"); // alloc: Cold
        }
        if h.flags.pure {
            out.push_str(" pure"); // alloc: Cold
        }
        if h.flags.touches_memory {
            out.push_str(" touches_memory"); // alloc: Cold
        }
        if h.flags.state != StateMask::ALL {
            let _ = write!(out, " state={}", h.flags.state.0); // alloc: Cold
        }
        out.push_str(")\n"); // alloc: Cold
    }
    for f in &p.funcs {
        print_function(&mut out, p, f);
    }
    out.push_str(")\n"); // alloc: Cold
    out
}

fn print_function(out: &mut String, p: &Program, f: &Function) { // alloc: Cold
    let _ = write!(out, "  (fn {} (", f.name); // alloc: Cold
    let entry = &f.blocks[f.entry.index()];
    join(out, entry.params.iter(), |o, &v| {
        let _ = write!(o, "(v{} {})", v.0, ty_name(f.values[v.index()])); // alloc: Cold
    });
    out.push_str(") ("); // alloc: Cold
    join(out, f.returns.iter(), |o, &t| o.push_str(ty_name(t))); // alloc: Cold
    out.push_str(")\n"); // alloc: Cold
    if !f.stack_slots.is_empty() {
        out.push_str("    (slots"); // alloc: Cold
        for s in &f.stack_slots {
            let _ = write!(out, " ({} {})", s.bytes, s.align); // alloc: Cold
        }
        out.push_str(")\n"); // alloc: Cold
    }
    let h = &f.hints;
    if !h.is_empty() {
        out.push_str("    (hints"); // alloc: Cold
        if !h.cold_blocks.is_empty() {
            out.push_str(" (cold"); // alloc: Cold
            for b in &h.cold_blocks { let _ = write!(out, " b{}", b.0); } // alloc: Cold
            out.push(')'); // alloc: Cold
        }
        for (b, fr) in &h.block_freq { let _ = write!(out, " (freq b{} {})", b.0, fr); } // alloc: Cold
        if !h.loop_headers.is_empty() {
            out.push_str(" (loop"); // alloc: Cold
            for b in &h.loop_headers { let _ = write!(out, " b{}", b.0); } // alloc: Cold
            out.push(')'); // alloc: Cold
        }
        for (b, then) in &h.likely { let _ = write!(out, " (likely b{} {})", b.0, if *then { "then" } else { "else" }); } // alloc: Cold
        if !h.remat.is_empty() {
            out.push_str(" (remat"); // alloc: Cold
            for v in &h.remat { let _ = write!(out, " v{}", v.0); } // alloc: Cold
            out.push(')'); // alloc: Cold
        }
        if !h.nowrap.is_empty() {
            out.push_str(" (nowrap"); // alloc: Cold
            for v in &h.nowrap { let _ = write!(out, " v{}", v.0); } // alloc: Cold
            out.push(')'); // alloc: Cold
        }
        out.push_str(")\n"); // alloc: Cold
    }
    // Entry block first so the loader can recover the signature from it.
    let order = core::iter::once(f.entry.index()).chain((0..f.blocks.len()).filter(|&i| i != f.entry.index()));
    for bi in order {
        let b = &f.blocks[bi];
        let _ = write!(out, "    (block b{} (", bi); // alloc: Cold
        join(out, b.params.iter(), |o, &v| {
            let _ = write!(o, "(v{} {})", v.0, ty_name(f.values[v.index()])); // alloc: Cold
        });
        out.push_str(")\n"); // alloc: Cold
        for inst in &b.insts {
            out.push_str("      ("); // alloc: Cold
            if let Some(r) = inst.result {
                let _ = write!(out, "v{} ", r.0); // alloc: Cold
            }
            print_op(out, p, &inst.op);
            out.push_str(")\n"); // alloc: Cold
        }
        out.push_str("      "); // alloc: Cold
        print_term(out, &b.term);
        out.push_str(")\n"); // alloc: Cold
    }
    out.push_str("  )\n"); // alloc: Cold
}

fn print_op(out: &mut String, p: &Program, op: &Op) { // alloc: Cold
    let v = |x: &Value| format!("v{}", x.0); // alloc: Cold
    let s = match op {
        Op::Const { ty, bits } => format!("(const {} {})", ty_name(*ty), const_lit(*ty, *bits)), // alloc: Cold
        Op::IntBin { op, a, b } => format!("({} {} {})", int_bin_name(*op), v(a), v(b)), // alloc: Cold
        Op::IntOvf { op, a, b } => format!("({} {} {})", int_ovf_name(*op), v(a), v(b)), // alloc: Cold
        Op::IntUn { op, a } => format!("({} {})", int_un_name(*op), v(a)), // alloc: Cold
        Op::Icmp { cond, a, b } => format!("(icmp {} {} {})", int_cond_name(*cond), v(a), v(b)), // alloc: Cold
        Op::FloatBin { op, a, b } => format!("({} {} {})", float_bin_name(*op), v(a), v(b)), // alloc: Cold
        Op::FloatUn { op, a } => format!("({} {})", float_un_name(*op), v(a)), // alloc: Cold
        Op::Fcmp { cond, a, b } => format!("(fcmp {} {} {})", float_cond_name(*cond), v(a), v(b)), // alloc: Cold
        Op::Conv { op, to, a } => match op {
            ConvOp::Fpromote | ConvOp::Fdemote | ConvOp::PtrToInt | ConvOp::IntToPtr => format!("({} {})", conv_name(*op), v(a)), // alloc: Cold
            _ => format!("({} {} {})", conv_name(*op), ty_name(*to), v(a)), // alloc: Cold
        },
        Op::Select { cond, a, b } => format!("(select {} {} {})", v(cond), v(a), v(b)), // alloc: Cold
        Op::Load { ty, ptr, offset, unaligned } => {
            format!("(load {} {} {}{})", ty_name(*ty), v(ptr), offset, if *unaligned { " unaligned" } else { "" }) // alloc: Cold
        }
        Op::LoadZext { ty, ptr, offset, unaligned } => {
            format!("(loadz {} {} {}{})", ty_name(*ty), v(ptr), offset, if *unaligned { " unaligned" } else { "" }) // alloc: Cold
        }
        Op::Store { ptr, offset, value, unaligned } => {
            format!("(store {} {} {}{})", v(ptr), offset, v(value), if *unaligned { " unaligned" } else { "" }) // alloc: Cold
        }
        Op::Prefetch { ptr, offset } => format!("(prefetch {} {})", v(ptr), offset), // alloc: Cold
        Op::Crc32c { acc, data } => format!("(crc32c {} {})", v(acc), v(data)), // alloc: Cold
        Op::AddHi { alo, blo, ahi, bhi } => format!("(addhi {} {} {} {})", v(alo), v(blo), v(ahi), v(bhi)), // alloc: Cold
        Op::StackAddr { slot } => format!("(stack_addr {})", slot.0), // alloc: Cold
        Op::PtrAdd { base, index, scale, offset } => format!("(ptr_add {} {} {} {})", v(base), v(index), scale, offset), // alloc: Cold
        Op::Call { callee, args } => {
            let mut s = format!("(call {}", p.funcs[callee.index()].name); // alloc: Cold
            for a in args.iter() {
                s.push(' '); // alloc: Cold
                s.push_str(&v(a)); // alloc: Cold
            }
            s.push(')'); // alloc: Cold
            s
        }
        Op::CallHost { callee, args } => {
            let mut s = format!("(call_host {}", p.host_fns[callee.index()].name); // alloc: Cold
            for a in args.iter() {
                s.push(' '); // alloc: Cold
                s.push_str(&v(a)); // alloc: Cold
            }
            s.push(')'); // alloc: Cold
            s
        }
        Op::VecLoad { ty, ptr, offset, unaligned } => {
            format!("(vload {} {} {}{})", ty_name(*ty), v(ptr), offset, if *unaligned { " unaligned" } else { "" }) // alloc: Cold
        }
        Op::VecStore { ptr, offset, value, unaligned } => {
            format!("(vstore {} {} {}{})", v(ptr), offset, v(value), if *unaligned { " unaligned" } else { "" }) // alloc: Cold
        }
        Op::VecBin { op, a, b } => format!("({} {} {})", vec_bin_name(*op), v(a), v(b)), // alloc: Cold
        Op::VecShift { op, a, count } => format!("({} {} {})", vec_shift_name(*op), v(a), count), // alloc: Cold
        Op::VecCmp { cond, a, b } => format!("(vcmp {} {} {})", int_cond_name(*cond), v(a), v(b)), // alloc: Cold
        Op::VecSelect { mask, a, b } => format!("(vselect {} {} {})", v(mask), v(a), v(b)), // alloc: Cold
        Op::VecReduceAdd { a } => format!("(vreduce_add {})", v(a)), // alloc: Cold
        Op::VecMovmask { a } => format!("(vmovmask {})", v(a)), // alloc: Cold
        Op::VecSplat { ty, a } => format!("(vsplat {} {})", ty_name(*ty), v(a)), // alloc: Cold
        Op::VecExtract { a, lane } => format!("(vextract {} {})", v(a), lane), // alloc: Cold
        Op::VecInsert { a, lane, value } => format!("(vinsert {} {} {})", v(a), lane, v(value)), // alloc: Cold
        Op::VecWiden { a, signed, high } => format!("(vwiden {} {} {})", if *signed { "s" } else { "u" }, if *high { "hi" } else { "lo" }, v(a)), // alloc: Cold
        Op::VecNarrow { a, b } => format!("(vnarrow {} {})", v(a), v(b)), // alloc: Cold
    };
    out.push_str(&s); // alloc: Cold
}

fn print_term(out: &mut String, t: &Terminator) { // alloc: Cold
    match t {
        Terminator::Br(e) => {
            let _ = write!(out, "(br b{}", e.target.0); // alloc: Cold
            for a in e.args.iter() {
                let _ = write!(out, " v{}", a.0); // alloc: Cold
            }
            out.push(')'); // alloc: Cold
        }
        Terminator::CondBr { cond, then_edge, else_edge } => {
            let _ = write!(out, "(condbr v{} ", cond.0); // alloc: Cold
            edge(out, then_edge);
            out.push(' '); // alloc: Cold
            edge(out, else_edge);
            out.push(')'); // alloc: Cold
        }
        Terminator::Switch { value, cases, default } => {
            let _ = write!(out, "(switch v{}", value.0); // alloc: Cold
            for c in cases.iter() {
                let _ = write!(out, " (case {} ", c.value); // alloc: Cold
                edge(out, &c.edge);
                out.push(')'); // alloc: Cold
            }
            out.push_str(" (default "); // alloc: Cold
            edge(out, default);
            out.push_str("))"); // alloc: Cold
        }
        Terminator::Return(vals) => {
            out.push_str("(return"); // alloc: Cold
            for a in vals.iter() {
                let _ = write!(out, " v{}", a.0); // alloc: Cold
            }
            out.push(')'); // alloc: Cold
        }
        Terminator::Unreachable => out.push_str("(unreachable)"), // alloc: Cold
    }
}

// ----------------------------------------------------------------------------
// S-expression reader
// ----------------------------------------------------------------------------

#[cfg(any(test, feature = "text-loader"))]
#[derive(Clone, Debug, PartialEq)]
pub enum Sexp {
    Atom(String),
    List(Vec<Sexp>),
}

#[cfg(any(test, feature = "text-loader"))]
impl Sexp {
    fn atom(&self) -> Result<&str, String> { // alloc: Cold
        match self {
            Sexp::Atom(s) => Ok(s),
            Sexp::List(_) => Err("expected atom, got list".into()),
        }
    }
    fn list(&self) -> Result<&[Sexp], String> { // alloc: Cold
        match self {
            Sexp::List(l) => Ok(l),
            Sexp::Atom(a) => Err(format!("expected list, got atom `{a}`")), // alloc: Cold
        }
    }
}

/// The deepest list nesting [`read_sexp`] reads (C6 (b)): the IR text grammar nests six lists at most
/// (program, function, block, instruction, operation, operand list); deeper input is an error, not a
/// recursion (and a recursive drop of the tree) that runs the thread off its stack.
#[cfg(any(test, feature = "text-loader"))]
pub const MAX_SEXP_DEPTH: u32 = 64;

#[cfg(any(test, feature = "text-loader"))]
pub fn read_sexp(src: &str) -> Result<Sexp, String> { // alloc: Cold
    let mut chars = src.char_indices().peekable();
    let v = read_one(src, &mut chars, 0)?.ok_or("empty input")?;
    skip_ws(src, &mut chars);
    if chars.peek().is_some() {
        return Err("trailing input after s-expression".into());
    }
    Ok(v)
}

#[cfg(any(test, feature = "text-loader"))]
type Chars<'a> = core::iter::Peekable<core::str::CharIndices<'a>>;

#[cfg(any(test, feature = "text-loader"))]
fn skip_ws(_src: &str, it: &mut Chars) {
    while let Some(&(_, c)) = it.peek() {
        if c.is_whitespace() {
            it.next();
        } else if c == ';' {
            while let Some(&(_, c)) = it.peek() {
                it.next();
                if c == '\n' {
                    break;
                }
            }
        } else {
            break;
        }
    }
}

#[cfg(any(test, feature = "text-loader"))]
fn read_one(src: &str, it: &mut Chars, depth: u32) -> Result<Option<Sexp>, String> { // alloc: Cold
    skip_ws(src, it);
    let Some(&(start, c)) = it.peek() else { return Ok(None) };
    if c == '(' {
        if depth >= MAX_SEXP_DEPTH {
            return Err(format!("lists nested deeper than {MAX_SEXP_DEPTH} at byte {start}")); // alloc: Cold
        }
        it.next();
        let mut items = Vec::new();
        loop {
            skip_ws(src, it);
            match it.peek() {
                Some(&(_, ')')) => {
                    it.next();
                    return Ok(Some(Sexp::List(items)));
                }
                Some(_) => items.push(read_one(src, it, depth + 1)?.ok_or("unterminated list")?),
                None => return Err("unterminated list".into()),
            }
        }
    }
    if c == ')' {
        return Err("unexpected `)`".into());
    }
    let mut end = start;
    while let Some(&(i, c)) = it.peek() {
        if c.is_whitespace() || c == '(' || c == ')' || c == ';' {
            break;
        }
        end = i + c.len_utf8();
        it.next();
    }
    Ok(Some(Sexp::Atom(src[start..end].to_string())))
}

// ----------------------------------------------------------------------------
// Loader
// ----------------------------------------------------------------------------

/// Resolve host function addresses by name. Tests that only print or
/// interpret with a custom host table can use `|_| 0`.
#[cfg(any(test, feature = "text-loader"))]
pub type HostResolver<'a> = &'a dyn Fn(&str) -> Option<usize>;

#[cfg(any(test, feature = "text-loader"))]
pub fn parse_program(src: &str, resolve_host: HostResolver) -> Result<Program, String> { // alloc: Cold
    let sexp = read_sexp(src)?;
    let items = sexp.list()?;
    if items.first().and_then(|s| s.atom().ok()) != Some("program") {
        return Err("expected (program ...)".into());
    }
    let mut pb = ProgramBuilder::new();
    let mut hosts: BTreeMap<String, (HostFn, Option<Type>)> = BTreeMap::new();
    let mut funcs: BTreeMap<String, (Func, Vec<Type>)> = BTreeMap::new();
    let mut fn_forms: Vec<&[Sexp]> = Vec::new();
    // Pass 1: declarations, so calls can be forward references.
    for item in &items[1..] {
        let l = item.list()?;
        match l.first().ok_or("empty form")?.atom()? {
            "host" => {
                let name = l.get(1).ok_or("host: missing name")?.atom()?;
                let params = parse_types(l.get(2).ok_or("host: missing params")?)?;
                let ret = match l.get(3).ok_or("host: missing return type")?.atom()? {
                    "void" => None,
                    t => Some(parse_type(t)?),
                };
                let mut flags = HostFlags::default();
                for f in &l[4..] {
                    match f.atom()? {
                        "may_error" => flags.may_error = true,
                        "pure" => flags.pure = true,
                        "touches_memory" => flags.touches_memory = true,
                        other => match other.strip_prefix("state=") {
                            Some(m) => flags.state = StateMask(m.parse::<u8>().map_err(|_| format!("host {name}: bad state mask `{m}`"))?), // alloc: Cold
                            None => return Err(format!("host {name}: unknown flag `{other}`")), // alloc: Cold
                        },
                    }
                }
                let addr = resolve_host(name).unwrap_or(0);
                hosts.insert(name.to_string(), (pb.declare_host(name, &params, ret, addr, flags), ret));
            }
            "fn" => {
                let name = l.get(1).ok_or("fn: missing name")?.atom()?;
                let params: Vec<Type> = l
                    .get(2)
                    .ok_or("fn: missing params")?
                    .list()?
                    .iter()
                    .map(|p| {
                        let p = p.list()?;
                        parse_type(p.get(1).ok_or("param: missing type")?.atom()?)
                    })
                    .collect::<Result<_, _>>()?;
                let returns = parse_types(l.get(3).ok_or("fn: missing returns")?)?;
                funcs.insert(name.to_string(), (pb.declare_func(name, &params, &returns), returns));
                fn_forms.push(l);
            }
            other => return Err(format!("unknown top-level form `{other}`")), // alloc: Cold
        }
    }
    // Pass 2: bodies.
    for l in fn_forms {
        let name = l[1].atom()?;
        let func = funcs[name].0;
        let fb = pb.define(func);
        load_body(fb, l, &funcs, &hosts).map_err(|e| format!("fn {name}: {e}"))?; // alloc: Cold
    }
    let program = pb.finish();
    crate::verify::verify(&program).map_err(|e| format!("fn {}: {}", e.func, e.message))?; // alloc: Cold
    Ok(program)
}

#[cfg(any(test, feature = "text-loader"))]
fn parse_type(s: &str) -> Result<Type, String> { // alloc: Cold
    Ok(match s {
        "bool" => Type::Bool,
        "i8" => Type::I8,
        "i16" => Type::I16,
        "i32" => Type::I32,
        "i64" => Type::I64,
        "f32" => Type::F32,
        "f64" => Type::F64,
        "ptr" => Type::Ptr,
        "v16i8" => Type::V16I8,
        "v8i16" => Type::V8I16,
        "v4i32" => Type::V4I32,
        "v2i64" => Type::V2I64,
        "v4f32" => Type::V4F32,
        "v2f64" => Type::V2F64,
        other => return Err(format!("unknown type `{other}`")), // alloc: Cold
    })
}

#[cfg(any(test, feature = "text-loader"))]
fn parse_types(s: &Sexp) -> Result<Vec<Type>, String> {
    s.list()?.iter().map(|t| parse_type(t.atom()?)).collect()
}

#[cfg(any(test, feature = "text-loader"))]
fn parse_int(s: &str) -> Result<i64, String> { // alloc: Cold
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s),
    };
    let v = if let Some(h) = body.strip_prefix("0x") {
        u64::from_str_radix(h, 16).map_err(|e| format!("bad integer `{s}`: {e}"))? // alloc: Cold
    } else {
        body.parse::<u64>().map_err(|e| format!("bad integer `{s}`: {e}"))? // alloc: Cold
    };
    Ok(if neg { (v as i64).wrapping_neg() } else { v as i64 })
}

#[cfg(any(test, feature = "text-loader"))]
fn parse_const(ty: Type, s: &str) -> Result<u64, String> { // alloc: Cold
    Ok(match ty {
        Type::Bool => match s {
            "true" | "1" => 1,
            "false" | "0" => 0,
            _ => return Err(format!("bad bool `{s}`")), // alloc: Cold
        },
        Type::F32 => s.parse::<f32>().map_err(|e| format!("bad f32 `{s}`: {e}"))?.to_bits() as u64, // alloc: Cold
        Type::F64 => s.parse::<f64>().map_err(|e| format!("bad f64 `{s}`: {e}"))?.to_bits(), // alloc: Cold
        _ => parse_int(s)? as u64,
    })
}

#[cfg(any(test, feature = "text-loader"))]
struct Names {
    values: BTreeMap<String, Value>,
    blocks: BTreeMap<String, Block>,
}

#[cfg(any(test, feature = "text-loader"))]
impl Names {
    fn value(&self, s: &Sexp) -> Result<Value, String> { // alloc: Cold
        let a = s.atom()?;
        self.values.get(a).copied().ok_or_else(|| format!("unknown value `{a}`")) // alloc: Cold
    }
    fn block(&self, s: &Sexp) -> Result<Block, String> { // alloc: Cold
        let a = s.atom()?;
        self.blocks.get(a).copied().ok_or_else(|| format!("unknown block `{a}`")) // alloc: Cold
    }
    fn edge(&self, s: &Sexp) -> Result<(Block, Vec<Value>), String> {
        let l = s.list()?;
        let target = self.block(l.first().ok_or("empty edge")?)?;
        let args = l[1..].iter().map(|a| self.value(a)).collect::<Result<_, _>>()?;
        Ok((target, args))
    }
}

#[cfg(any(test, feature = "text-loader"))]
fn load_body(
    mut fb: FunctionBuilder<'_>,
    l: &[Sexp],
    funcs: &BTreeMap<String, (Func, Vec<Type>)>,
    hosts: &BTreeMap<String, (HostFn, Option<Type>)>,
) -> Result<(), String> { // alloc: Cold
    let mut names = Names { values: BTreeMap::new(), blocks: BTreeMap::new() };
    let mut body = &l[4..];
    if let Some(first) = body.first() {
        if first.list()?.first().and_then(|a| a.atom().ok()) == Some("slots") {
            for s in &first.list()?[1..] {
                let s = s.list()?;
                let bytes = parse_int(s.first().ok_or("slot: bytes")?.atom()?)? as u32;
                let align = parse_int(s.get(1).ok_or("slot: align")?.atom()?)? as u32;
                fb.declare_stack_slot(bytes, align);
            }
            body = &body[1..];
        }
    }
    // An optional (hints ...) form before the blocks; applied after the
    // blocks and values exist.
    let mut hints_form: Option<&[Sexp]> = None;
    if let Some(first) = body.first() {
        if first.list()?.first().and_then(|a| a.atom().ok()) == Some("hints") {
            hints_form = Some(&first.list()?[1..]);
            body = &body[1..];
        }
    }
    // Pass A: create blocks. The first block is the entry, which `define`
    // already created; bind its params to the names in the form.
    let mut block_forms: Vec<(&[Sexp], Block)> = Vec::new();
    for (i, form) in body.iter().enumerate() {
        let form = form.list()?;
        if form.first().and_then(|a| a.atom().ok()) != Some("block") {
            return Err("expected (block ...)".into());
        }
        let bname = form.get(1).ok_or("block: missing name")?.atom()?;
        let params = form.get(2).ok_or("block: missing params")?.list()?;
        let param_types: Vec<Type> =
            params.iter().map(|p| parse_type(p.list()?.get(1).ok_or("param: type")?.atom()?)).collect::<Result<_, _>>()?;
        let block = if i == 0 {
            let e = fb.entry();
            if fb.block_params(e).iter().map(|&v| fb.value_type(v)).collect::<Vec<_>>() != param_types {
                return Err("entry block params do not match fn params".into());
            }
            e
        } else {
            fb.create_block(&param_types)
        };
        for (p, &v) in params.iter().zip(fb.block_params(block).to_vec().iter()) {
            let pname = p.list()?.first().ok_or("param: name")?.atom()?;
            if names.values.insert(pname.to_string(), v).is_some() {
                return Err(format!("value `{pname}` defined twice")); // alloc: Cold
            }
        }
        if names.blocks.insert(bname.to_string(), block).is_some() {
            return Err(format!("block `{bname}` defined twice")); // alloc: Cold
        }
        block_forms.push((form, block));
    }
    // Pass B: instructions. Values must be defined textually before use,
    // except block parameters, which are all known.
    for (form, block) in block_forms {
        fb.switch_to(block);
        let items = &form[3..];
        let (term, insts) = items.split_last().ok_or("block: missing terminator")?;
        for inst in insts {
            let inst = inst.list()?;
            let (result_name, op) = match inst.first().ok_or("empty instruction")? {
                Sexp::Atom(name) => (Some(name.as_str()), inst.get(1).ok_or("instruction: missing op")?.list()?),
                Sexp::List(op) => (None, op.as_slice()),
            };
            let result = load_op(&mut fb, op, &names, funcs, hosts)?;
            match (result_name, result) {
                (Some(n), Some(v)) => {
                    if names.values.insert(n.to_string(), v).is_some() {
                        return Err(format!("value `{n}` defined twice")); // alloc: Cold
                    }
                }
                (None, None) => {}
                (Some(n), None) => return Err(format!("`{n}`: op produces no value")), // alloc: Cold
                (None, Some(_)) => return Err("op produces a value but no name given".into()),
            }
        }
        load_term(&mut fb, term.list()?, &names)?;
    }
    if let Some(h) = hints_form {
        for item in h {
            let item = item.list()?;
            let kind = item.first().ok_or("hint: empty")?.atom()?;
            let blk = |k: &Sexp| -> Result<Block, String> { let n = k.atom()?; names.blocks.get(n).copied().ok_or_else(|| format!("hint: unknown block `{n}`")) }; // alloc: Cold
            let val = |k: &Sexp| -> Result<Value, String> { let n = k.atom()?; names.values.get(n).copied().ok_or_else(|| format!("hint: unknown value `{n}`")) }; // alloc: Cold
            match kind {
                "cold" => for k in &item[1..] { let b = blk(k)?; fb.mark_cold(b); },
                "loop" => for k in &item[1..] { let b = blk(k)?; fb.mark_loop_header(b); },
                "remat" => for k in &item[1..] { let v = val(k)?; fb.mark_remat(v); },
                "nowrap" => for k in &item[1..] { let v = val(k)?; fb.mark_nowrap(v); },
                "freq" => {
                    let b = blk(item.get(1).ok_or("freq: block")?)?;
                    let f: f32 = item.get(2).ok_or("freq: value")?.atom()?.parse().map_err(|_| "freq: not a number".to_string())?; // alloc: Cold
                    fb.set_freq(b, f);
                }
                "likely" => {
                    let b = blk(item.get(1).ok_or("likely: block")?)?;
                    let which = item.get(2).ok_or("likely: edge")?.atom()?;
                    fb.set_likely(b, which == "then");
                }
                other => return Err(format!("unknown hint `{other}`")), // alloc: Cold
            }
        }
    }
    Ok(())
}

#[cfg(any(test, feature = "text-loader"))]
fn load_op(
    fb: &mut FunctionBuilder<'_>,
    op: &[Sexp],
    n: &Names,
    funcs: &BTreeMap<String, (Func, Vec<Type>)>,
    hosts: &BTreeMap<String, (HostFn, Option<Type>)>,
) -> Result<Option<Value>, String> { // alloc: Cold
    let head = op.first().ok_or("empty op")?.atom()?;
    let arg = |i: usize| -> Result<&Sexp, String> { op.get(i).ok_or_else(|| format!("{head}: missing operand {i}")) }; // alloc: Cold
    let v = |i: usize| -> Result<Value, String> { n.value(arg(i)?) }; // alloc: Cold
    let int_bin = |o: IntBinOp, fb: &mut FunctionBuilder| Ok(Some(fb.int_bin(o, v(1)?, v(2)?)));
    let int_un = |o: IntUnOp, fb: &mut FunctionBuilder| Ok(Some(fb.int_un(o, v(1)?)));
    let int_ovf = |o: IntOvfOp, fb: &mut FunctionBuilder| Ok(Some(fb.int_ovf(o, v(1)?, v(2)?)));
    let float_bin = |o: FloatBinOp, fb: &mut FunctionBuilder| Ok(Some(fb.float_bin(o, v(1)?, v(2)?)));
    let float_un = |o: FloatUnOp, fb: &mut FunctionBuilder| Ok(Some(fb.float_un(o, v(1)?)));
    let conv_to = |o: ConvOp, fb: &mut FunctionBuilder| -> Result<Option<Value>, String> { // alloc: Cold
        let to = parse_type(arg(1)?.atom()?)?;
        Ok(Some(fb.conv(o, to, v(2)?)))
    };
    let conv_fixed = |o: ConvOp, to: Type, fb: &mut FunctionBuilder| Ok(Some(fb.conv(o, to, v(1)?)));
    match head {
        "const" => {
            let ty = parse_type(arg(1)?.atom()?)?;
            let bits = parse_const(ty, arg(2)?.atom()?)?;
            Ok(Some(fb.const_raw(ty, bits)))
        }
        "iadd" => int_bin(IntBinOp::Add, fb),
        "isub" => int_bin(IntBinOp::Sub, fb),
        "imul" => int_bin(IntBinOp::Mul, fb),
        "sdiv" => int_bin(IntBinOp::Sdiv, fb),
        "udiv" => int_bin(IntBinOp::Udiv, fb),
        "srem" => int_bin(IntBinOp::Srem, fb),
        "urem" => int_bin(IntBinOp::Urem, fb),
        "iand" => int_bin(IntBinOp::And, fb),
        "ior" => int_bin(IntBinOp::Or, fb),
        "ixor" => int_bin(IntBinOp::Xor, fb),
        "shl" => int_bin(IntBinOp::Shl, fb),
        "ushr" => int_bin(IntBinOp::Ushr, fb),
        "sshr" => int_bin(IntBinOp::Sshr, fb),
        "smin" => int_bin(IntBinOp::Smin, fb),
        "smax" => int_bin(IntBinOp::Smax, fb),
        "umin" => int_bin(IntBinOp::Umin, fb),
        "umax" => int_bin(IntBinOp::Umax, fb),
        "smulh" => int_bin(IntBinOp::Smulh, fb),
        "umulh" => int_bin(IntBinOp::Umulh, fb),
        "rotl" => int_bin(IntBinOp::Rotl, fb),
        "sadd_ovf" => int_ovf(IntOvfOp::SaddOvf, fb),
        "ssub_ovf" => int_ovf(IntOvfOp::SsubOvf, fb),
        "smul_ovf" => int_ovf(IntOvfOp::SmulOvf, fb),
        "ineg" => int_un(IntUnOp::Neg, fb),
        "inot" => int_un(IntUnOp::Not, fb),
        "clz" => int_un(IntUnOp::Clz, fb),
        "ctz" => int_un(IntUnOp::Ctz, fb),
        "popcnt" => int_un(IntUnOp::Popcnt, fb),
        "bswap" => int_un(IntUnOp::Bswap, fb),
        "bitrev" => int_un(IntUnOp::Bitrev, fb),
        "icmp" => {
            let cond = parse_int_cond(arg(1)?.atom()?)?;
            Ok(Some(fb.icmp(cond, v(2)?, v(3)?)))
        }
        "fadd" => float_bin(FloatBinOp::Add, fb),
        "fsub" => float_bin(FloatBinOp::Sub, fb),
        "fmul" => float_bin(FloatBinOp::Mul, fb),
        "fdiv" => float_bin(FloatBinOp::Div, fb),
        "fmin" => float_bin(FloatBinOp::Min, fb),
        "fmax" => float_bin(FloatBinOp::Max, fb),
        "copysign" => float_bin(FloatBinOp::Copysign, fb),
        "fneg" => float_un(FloatUnOp::Neg, fb),
        "fabs" => float_un(FloatUnOp::Abs, fb),
        "sqrt" => float_un(FloatUnOp::Sqrt, fb),
        "ceil" => float_un(FloatUnOp::Ceil, fb),
        "floor" => float_un(FloatUnOp::Floor, fb),
        "trunc" => float_un(FloatUnOp::Trunc, fb),
        "nearest" => float_un(FloatUnOp::Nearest, fb),
        "fcmp" => {
            let cond = match arg(1)?.atom()? {
                "eq" => FloatCond::Eq,
                "ne" => FloatCond::Ne,
                "lt" => FloatCond::Lt,
                "le" => FloatCond::Le,
                "gt" => FloatCond::Gt,
                "ge" => FloatCond::Ge,
                "ord" => FloatCond::Ordered,
                "uno" => FloatCond::Unordered,
                c => return Err(format!("fcmp: unknown condition `{c}`")), // alloc: Cold
            };
            Ok(Some(fb.fcmp(cond, v(2)?, v(3)?)))
        }
        "sext" => conv_to(ConvOp::Sext, fb),
        "zext" => conv_to(ConvOp::Zext, fb),
        "itrunc" => conv_to(ConvOp::Trunc, fb),
        "sitofp" => conv_to(ConvOp::SintToFloat, fb),
        "uitofp" => conv_to(ConvOp::UintToFloat, fb),
        "fptosi" => conv_to(ConvOp::FloatToSint, fb),
        "fptoui" => conv_to(ConvOp::FloatToUint, fb),
        "bitcast" => conv_to(ConvOp::Bitcast, fb),
        "fpromote" => conv_fixed(ConvOp::Fpromote, Type::F64, fb),
        "fdemote" => conv_fixed(ConvOp::Fdemote, Type::F32, fb),
        "ptrtoint" => conv_fixed(ConvOp::PtrToInt, Type::I64, fb),
        "inttoptr" => conv_fixed(ConvOp::IntToPtr, Type::Ptr, fb),
        "select" => Ok(Some(fb.select(v(1)?, v(2)?, v(3)?))),
        "load" => {
            let ty = parse_type(arg(1)?.atom()?)?;
            let ptr = v(2)?;
            let off = parse_int(arg(3)?.atom()?)? as i32;
            let unaligned = op.get(4).map(|a| a.atom()) == Some(Ok("unaligned"));
            Ok(Some(if unaligned { fb.load_unaligned(ty, ptr, off) } else { fb.load(ty, ptr, off) }))
        }
        "loadz" => {
            let ty = parse_type(arg(1)?.atom()?)?;
            let ptr = v(2)?;
            let off = parse_int(arg(3)?.atom()?)? as i32;
            let unaligned = op.get(4).map(|a| a.atom()) == Some(Ok("unaligned"));
            let val = fb.load_zext(ty, ptr, off);
            if unaligned {
                if let Some(crate::Instruction { op: Op::LoadZext { unaligned: u, .. }, .. }) = fb.last_inst_mut() {
                    *u = true;
                }
            }
            Ok(Some(val))
        }
        "store" => {
            let ptr = v(1)?;
            let off = parse_int(arg(2)?.atom()?)? as i32;
            let val = v(3)?;
            let unaligned = op.get(4).map(|a| a.atom()) == Some(Ok("unaligned"));
            if unaligned {
                fb.store_unaligned(ptr, off, val)
            } else {
                fb.store(ptr, off, val)
            }
            Ok(None)
        }
        "prefetch" => {
            let ptr = v(1)?;
            let off = parse_int(arg(2)?.atom()?)? as i32;
            fb.prefetch(ptr, off);
            Ok(None)
        }
        "crc32c" => Ok(Some(fb.crc32c(v(1)?, v(2)?))),
        "addhi" => Ok(Some(fb.add_hi(v(1)?, v(2)?, v(3)?, v(4)?))),
        "stack_addr" => {
            let slot = parse_int(arg(1)?.atom()?)? as u32;
            Ok(Some(fb.stack_addr(StackSlot(slot))))
        }
        "ptr_add" => {
            let scale = parse_int(arg(3)?.atom()?)? as u8;
            let off = parse_int(arg(4)?.atom()?)? as i32;
            Ok(Some(fb.ptr_add(v(1)?, v(2)?, scale, off)))
        }
        "call" => {
            let name = arg(1)?.atom()?;
            let (callee, returns) = funcs.get(name).ok_or_else(|| format!("unknown function `{name}`"))?; // alloc: Cold
            let ret = match returns.as_slice() {
                [] => None,
                [t] => Some(*t),
                _ => return Err(format!("call {name}: multi-value returns not supported")), // alloc: Cold
            };
            let args = op[2..].iter().map(|a| n.value(a)).collect::<Result<Vec<_>, _>>()?;
            Ok(fb.call(*callee, &args, ret))
        }
        "call_host" => {
            let name = arg(1)?.atom()?;
            let (callee, ret) = *hosts.get(name).ok_or_else(|| format!("unknown host fn `{name}`"))?; // alloc: Cold
            let args = op[2..].iter().map(|a| n.value(a)).collect::<Result<Vec<_>, _>>()?;
            Ok(fb.call_host(callee, &args, ret))
        }
        "vload" => {
            let ty = parse_type(arg(1)?.atom()?)?;
            let ptr = v(2)?;
            let off = parse_int(arg(3)?.atom()?)? as i32;
            let unaligned = op.get(4).map(|a| a.atom()) == Some(Ok("unaligned"));
            if !ty.is_vector() {
                return Err(format!("vload: {ty:?} is not a vector type")); // alloc: Cold
            }
            Ok(Some(if unaligned { fb.vload_unaligned(ty, ptr, off) } else { fb.vload(ty, ptr, off) }))
        }
        "vstore" => {
            let ptr = v(1)?;
            let off = parse_int(arg(2)?.atom()?)? as i32;
            let val = v(3)?;
            let unaligned = op.get(4).map(|a| a.atom()) == Some(Ok("unaligned"));
            if unaligned {
                fb.vstore_unaligned(ptr, off, val)
            } else {
                fb.vstore(ptr, off, val)
            }
            Ok(None)
        }
        "vadd" => Ok(Some(fb.vbin(VecBinOp::Add, v(1)?, v(2)?))),
        "vsub" => Ok(Some(fb.vbin(VecBinOp::Sub, v(1)?, v(2)?))),
        "vmul" => Ok(Some(fb.vbin(VecBinOp::Mul, v(1)?, v(2)?))),
        "vand" => Ok(Some(fb.vbin(VecBinOp::And, v(1)?, v(2)?))),
        "vor" => Ok(Some(fb.vbin(VecBinOp::Or, v(1)?, v(2)?))),
        "vxor" => Ok(Some(fb.vbin(VecBinOp::Xor, v(1)?, v(2)?))),
        "vsmin" => Ok(Some(fb.vbin(VecBinOp::Smin, v(1)?, v(2)?))),
        "vsmax" => Ok(Some(fb.vbin(VecBinOp::Smax, v(1)?, v(2)?))),
        "vumin" => Ok(Some(fb.vbin(VecBinOp::Umin, v(1)?, v(2)?))),
        "vumax" => Ok(Some(fb.vbin(VecBinOp::Umax, v(1)?, v(2)?))),
        "vshl" | "vushr" | "vsshr" => {
            let o = match head {
                "vshl" => VecShiftOp::Shl,
                "vushr" => VecShiftOp::Ushr,
                _ => VecShiftOp::Sshr,
            };
            let count = parse_int(arg(2)?.atom()?)? as u8;
            Ok(Some(fb.vshift(o, v(1)?, count)))
        }
        "vcmp" => {
            let cond = parse_int_cond(arg(1)?.atom()?)?;
            Ok(Some(fb.vcmp(cond, v(2)?, v(3)?)))
        }
        "vselect" => Ok(Some(fb.vselect(v(1)?, v(2)?, v(3)?))),
        "vreduce_add" => Ok(Some(fb.vreduce_add(v(1)?))),
        "vmovmask" => Ok(Some(fb.vmovmask(v(1)?))),
        "vsplat" => {
            let ty = parse_type(arg(1)?.atom()?)?;
            if !ty.is_vector() {
                return Err(format!("vsplat: {ty:?} is not a vector type")); // alloc: Cold
            }
            Ok(Some(fb.vsplat(ty, v(2)?)))
        }
        "vextract" => {
            let lane = parse_int(arg(2)?.atom()?)? as u8;
            Ok(Some(fb.vextract(v(1)?, lane)))
        }
        "vinsert" => {
            let lane = parse_int(arg(2)?.atom()?)? as u8;
            Ok(Some(fb.vinsert(v(1)?, lane, v(3)?)))
        }
        "vwiden" => {
            let signed = match arg(1)?.atom()? {
                "s" => true,
                "u" => false,
                o => return Err(format!("vwiden: expected s|u, got `{o}`")), // alloc: Cold
            };
            let high = match arg(2)?.atom()? {
                "hi" => true,
                "lo" => false,
                o => return Err(format!("vwiden: expected lo|hi, got `{o}`")), // alloc: Cold
            };
            let a = v(3)?;
            if fb.value_type(a).widened().is_none() {
                return Err(format!("vwiden: {:?} has no wider lanes", fb.value_type(a))); // alloc: Cold
            }
            Ok(Some(fb.vwiden(a, signed, high)))
        }
        "vnarrow" => {
            let a = v(1)?;
            if fb.value_type(a).narrowed().is_none() {
                return Err(format!("vnarrow: {:?} has no narrower lanes", fb.value_type(a))); // alloc: Cold
            }
            Ok(Some(fb.vnarrow(a, v(2)?)))
        }
        other => Err(format!("unknown op `{other}`")), // alloc: Cold
    }
}

#[cfg(any(test, feature = "text-loader"))]
fn parse_int_cond(s: &str) -> Result<IntCond, String> { // alloc: Cold
    Ok(match s {
        "eq" => IntCond::Eq,
        "ne" => IntCond::Ne,
        "slt" => IntCond::Slt,
        "sle" => IntCond::Sle,
        "sgt" => IntCond::Sgt,
        "sge" => IntCond::Sge,
        "ult" => IntCond::Ult,
        "ule" => IntCond::Ule,
        "ugt" => IntCond::Ugt,
        "uge" => IntCond::Uge,
        c => return Err(format!("unknown compare condition `{c}`")), // alloc: Cold
    })
}

#[cfg(any(test, feature = "text-loader"))]
fn load_term(fb: &mut FunctionBuilder<'_>, t: &[Sexp], n: &Names) -> Result<(), String> { // alloc: Cold
    let head = t.first().ok_or("empty terminator")?.atom()?;
    match head {
        "br" => {
            let target = n.block(t.get(1).ok_or("br: missing target")?)?;
            let args = t[2..].iter().map(|a| n.value(a)).collect::<Result<Vec<_>, _>>()?;
            fb.br(target, &args);
        }
        "condbr" => {
            let cond = n.value(t.get(1).ok_or("condbr: missing cond")?)?;
            let te = n.edge(t.get(2).ok_or("condbr: missing then edge")?)?;
            let ee = n.edge(t.get(3).ok_or("condbr: missing else edge")?)?;
            let (then_edge, else_edge) = (fb.edge(te.0, &te.1), fb.edge(ee.0, &ee.1));
            fb.terminate(Terminator::CondBr { cond, then_edge, else_edge });
        }
        "switch" => {
            let value = n.value(t.get(1).ok_or("switch: missing value")?)?;
            let mut cases = Vec::new();
            let mut default = None;
            for c in &t[2..] {
                let c = c.list()?;
                match c.first().ok_or("switch: empty case")?.atom()? {
                    "case" => {
                        let value = parse_int(c.get(1).ok_or("case: missing value")?.atom()?)? as u64;
                        let (target, args) = n.edge(c.get(2).ok_or("case: missing edge")?)?;
                        cases.push((value, target, args));
                    }
                    "default" => default = Some(n.edge(c.get(1).ok_or("default: missing edge")?)?),
                    o => return Err(format!("switch: unknown form `{o}`")), // alloc: Cold
                }
            }
            let (dt, da) = default.ok_or("switch: missing default")?;
            fb.switch(value, cases, (dt, &da));
        }
        "return" => {
            let vals = t[1..].iter().map(|a| n.value(a)).collect::<Result<Vec<_>, _>>()?;
            fb.ret(&vals);
        }
        "unreachable" => fb.unreachable(),
        other => return Err(format!("unknown terminator `{other}`")), // alloc: Cold
    }
    Ok(())
}
