//! Gravity IR: a small, trusted, in-process SSA IR for generated query code.
//!
//! Design rules:
//!
//! * The IR is produced by the lowerer and consumed by the interpreter and the
//!   TPDE backend in the same process. It is never serialized for admission and
//!   there is no verifier on the production path. A text (s-expression) form
//!   exists only for tests.
//! * Types are machine types. SQL semantics (NULL flags, overflow errors,
//!   Datum encodings, tuple layouts) live entirely in the lowerer, which emits
//!   explicit flags, branches and host calls.
//! * Every instruction defines at most one value. Operations that need a second
//!   output (overflow, division by zero) are separate predicate instructions the
//!   backend may fuse.
//! * Memory is `ptr + constant offset`. The lowerer computes offsets from
//!   `#[repr(C)]` state structs; the IR never describes a struct or a row.
//! * Generated code never suspends. A function runs to completion and returns.
//!   Resumability lives in the executor node's state struct and the Volcano loop.
//! * Generated code allocates through host calls and never owns memory.
//! * Control flow is a CFG of blocks with parameters; edges carry arguments.
//!   There are no phi nodes.
//!
//! Anything undefined below (division by zero, out-of-range float-to-int,
//! misaligned loads, shift counts >= width) is a lowerer bug, not a runtime
//! check. The lowerer emits guards where SQL requires an error.

#![no_std]
#![deny(unsafe_code)]
extern crate alloc;
use alloc::vec::Vec;
use mcx::SendRoot;

mod raw;

/// The allocator of every vector of a program: a handle into the program's own root (P3, `Program`).
pub use mcx::RootA as IrA;
pub type IrVec<T> = allocator_api2::vec::Vec<T, IrA>;
/// A list fixed when it is made (a call's or an edge's arguments, a block's parameters, a return's values, a switch's cases),
/// in the program's root: no capacity word (24 B, an `IrVec` is 32), so the blocks that hold them stay small (D4463). A rare
/// grower makes a new one (`Mint::appended`).
#[derive(PartialEq, Eq, Hash)]
pub struct IrSlice<T>(allocator_api2::boxed::Box<[T], IrA>);

impl<T> IrSlice<T> {
    pub fn from_vec(v: IrVec<T>) -> IrSlice<T> {
        IrSlice(v.into_boxed_slice())
    }
}

impl<T> core::ops::Deref for IrSlice<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T> core::ops::DerefMut for IrSlice<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.0
    }
}

impl<'a, T> IntoIterator for &'a IrSlice<T> {
    type Item = &'a T;
    type IntoIter = core::slice::Iter<'a, T>;
    fn into_iter(self) -> core::slice::Iter<'a, T> {
        self.0.iter()
    }
}

impl<'a, T> IntoIterator for &'a mut IrSlice<T> {
    type Item = &'a mut T;
    type IntoIter = core::slice::IterMut<'a, T>;
    fn into_iter(self) -> core::slice::IterMut<'a, T> {
        self.0.iter_mut()
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for IrSlice<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.0.iter()).finish()
    }
}

// ----------------------------------------------------------------------------
// Types
// ----------------------------------------------------------------------------

/// Machine value types. `Bool` is a distinct type so that branch conditions and
/// comparison results are not confused with integers; it is materialized as
/// one byte (0 or 1) in memory and in registers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Type {
    Bool,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    /// Untyped pointer, pointer-sized. Provenance is not tracked.
    Ptr,
    /// Fixed 128-bit vectors (`docs/design/gravity-simple/simd.md`): the
    /// lanes are numbered from the low bits (lane 0 is bits 0..w), which
    /// is the little-endian memory order of `vload` / `vstore` and the
    /// NEON / SSE lane order. Vector values never cross a function or
    /// host boundary (params, returns and host arguments are scalars);
    /// they live in block parameters, instructions and memory only.
    V16I8,
    V8I16,
    V4I32,
    V2I64,
    V4F32,
    V2F64,
}

impl Type {
    /// Size in bytes as stored in memory.
    pub const fn bytes(self) -> u32 {
        match self {
            Type::Bool | Type::I8 => 1,
            Type::I16 => 2,
            Type::I32 | Type::F32 => 4,
            Type::I64 | Type::F64 | Type::Ptr => 8,
            Type::V16I8 | Type::V8I16 | Type::V4I32 | Type::V2I64 | Type::V4F32 | Type::V2F64 => 16,
        }
    }
    /// Scalar integer (never a vector).
    pub const fn is_int(self) -> bool {
        matches!(self, Type::I8 | Type::I16 | Type::I32 | Type::I64)
    }
    /// Scalar float (never a vector).
    pub const fn is_float(self) -> bool {
        matches!(self, Type::F32 | Type::F64)
    }
    pub const fn is_vector(self) -> bool {
        matches!(self, Type::V16I8 | Type::V8I16 | Type::V4I32 | Type::V2I64 | Type::V4F32 | Type::V2F64)
    }
    pub const fn is_int_vector(self) -> bool {
        matches!(self, Type::V16I8 | Type::V8I16 | Type::V4I32 | Type::V2I64)
    }
    pub const fn is_float_vector(self) -> bool {
        matches!(self, Type::V4F32 | Type::V2F64)
    }
    /// Lane count of a vector type; 0 for a scalar.
    pub const fn lanes(self) -> u32 {
        match self {
            Type::V16I8 => 16,
            Type::V8I16 => 8,
            Type::V4I32 | Type::V4F32 => 4,
            Type::V2I64 | Type::V2F64 => 2,
            _ => 0,
        }
    }
    /// The scalar type of one lane; a scalar is its own lane type.
    pub const fn lane(self) -> Type {
        match self {
            Type::V16I8 => Type::I8,
            Type::V8I16 => Type::I16,
            Type::V4I32 => Type::I32,
            Type::V2I64 => Type::I64,
            Type::V4F32 => Type::F32,
            Type::V2F64 => Type::F64,
            t => t,
        }
    }
    /// The 128-bit vector whose lanes are `lane`, if there is one.
    pub const fn vector_of(lane: Type) -> Option<Type> {
        Some(match lane {
            Type::I8 => Type::V16I8,
            Type::I16 => Type::V8I16,
            Type::I32 => Type::V4I32,
            Type::I64 => Type::V2I64,
            Type::F32 => Type::V4F32,
            Type::F64 => Type::V2F64,
            _ => return None,
        })
    }
    /// The integer vector with lanes twice as wide (`VecWiden` result).
    pub const fn widened(self) -> Option<Type> {
        Some(match self {
            Type::V16I8 => Type::V8I16,
            Type::V8I16 => Type::V4I32,
            Type::V4I32 => Type::V2I64,
            _ => return None,
        })
    }
    /// The integer vector with lanes half as wide (`VecNarrow` result).
    pub const fn narrowed(self) -> Option<Type> {
        Some(match self {
            Type::V8I16 => Type::V16I8,
            Type::V4I32 => Type::V8I16,
            Type::V2I64 => Type::V4I32,
            _ => return None,
        })
    }
}

// ----------------------------------------------------------------------------
// Identifiers (indices into per-function or per-program tables)
// ----------------------------------------------------------------------------

macro_rules! id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
        pub struct $name(pub u32);
        impl $name {
            pub const fn index(self) -> usize { self.0 as usize }
        }
    };
}

id!(/// An SSA value, scoped to one function. Defined once: by a block
    /// parameter or by an instruction result.
    Value);
id!(/// A basic block, scoped to one function.
    Block);
id!(/// A function in the program.
    Func);
id!(/// A host (Rust) function declared in the program's host table.
    HostFn);
id!(/// A fixed-size stack slot declared by a function.
    StackSlot);

// ----------------------------------------------------------------------------
// Instructions
// ----------------------------------------------------------------------------

/// Integer comparison predicate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum IntCond {
    Eq,
    Ne,
    Slt,
    Sle,
    Sgt,
    Sge,
    Ult,
    Ule,
    Ugt,
    Uge,
}

/// Float comparison predicate. `Ordered` means neither operand is NaN;
/// `Unordered` means at least one is. The relational predicates are IEEE:
/// they are false whenever an operand is NaN, except `Ne`, which is true.
/// PostgreSQL's NaN-sorts-last ordering is composed by the lowerer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum FloatCond {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Ordered,
    Unordered,
}

/// Binary integer operations. Operands and result share one integer type.
/// `And`, `Or` and `Xor` additionally accept `Bool` operands, where they are
/// boolean logic. Arithmetic wraps modulo 2^width. Division and remainder are undefined for a
/// zero divisor and for `MIN / -1`; the lowerer guards those.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum IntBinOp {
    Add,
    Sub,
    Mul,
    Sdiv,
    Udiv,
    Srem,
    Urem,
    And,
    Or,
    Xor,
    /// Shift left. Count is taken modulo the width.
    Shl,
    /// Logical (zero-filling) shift right. Count is taken modulo the width.
    Ushr,
    /// Arithmetic (sign-filling) shift right. Count is taken modulo the width.
    Sshr,
    /// Signed / unsigned minimum and maximum of the two operands (never on `Bool`).
    Smin,
    Smax,
    Umin,
    Umax,
    /// The high word of the signed double-width product (`I64` only): `(a * b) >> 64` over i128 (`smulh` on AArch64).
    Smulh,
    /// The high word of the unsigned double-width product (`I64` only): `(a * b) >> 64` over u128 (`umulh` on AArch64).
    Umulh,
    /// Rotate left (`I64` only). Count is taken modulo the width.
    Rotl,
}

/// Integer overflow predicates: `Bool` result that is true iff the signed
/// operation on the two operands would not fit in their type. Paired with the
/// wrapping `IntBinOp` by the lowerer; backends may fuse the pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum IntOvfOp {
    SaddOvf,
    SsubOvf,
    SmulOvf,
}

/// Unary integer operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum IntUnOp {
    /// Two's-complement negation, wrapping.
    Neg,
    /// Bitwise complement.
    Not,
    Clz,
    Ctz,
    Popcnt,
    Bswap,
    /// The bits of the value reversed within its type's width
    /// (`HashAgg::partition_of`'s bit reverse): `rbit` on AArch64.
    Bitrev,
}

/// Binary float operations, IEEE 754 round-to-nearest-even. No fast-math.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum FloatBinOp {
    Add,
    Sub,
    Mul,
    Div,
    /// IEEE minNum / maxNum (NaN-propagating per Rust `f64::min`/`max`).
    Min,
    Max,
    /// Copy the sign of the second operand onto the first.
    Copysign,
}

/// Unary float operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum FloatUnOp {
    Neg,
    Abs,
    Sqrt,
    Ceil,
    Floor,
    Trunc,
    /// Round to nearest, ties to even.
    Nearest,
}

/// Conversions. The result type is carried on the instruction.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ConvOp {
    /// Integer to wider integer, sign-extending. Also `Bool` -> any int (0/1).
    Sext,
    /// Integer to wider integer, zero-extending. Also `Bool` -> any int (0/1).
    Zext,
    /// Integer to narrower integer, discarding high bits.
    Trunc,
    /// Signed integer to float, rounding to nearest even.
    SintToFloat,
    /// Unsigned integer to float, rounding to nearest even.
    UintToFloat,
    /// Float to signed integer, truncating toward zero. Undefined if the
    /// truncated value does not fit or the input is NaN; the lowerer guards.
    FloatToSint,
    /// Float to unsigned integer, truncating toward zero. Same caveats.
    FloatToUint,
    /// `F32` -> `F64`.
    Fpromote,
    /// `F64` -> `F32`, rounding to nearest even.
    Fdemote,
    /// Reinterpret bits between same-width int and float types.
    Bitcast,
    /// `Ptr` -> `I64`.
    PtrToInt,
    /// `I64` -> `Ptr`.
    IntToPtr,
}

/// Lane-wise binary vector operations. Operands and result share one
/// vector type. `Add`/`Sub`/`Mul` are wrapping on integer lanes and IEEE
/// round-to-nearest-even on float lanes; `Mul` is not defined on `V2I64`
/// (AArch64 NEON has no 64-bit lane multiply; the lowerer never needs
/// one). `And`/`Or`/`Xor` are bitwise over the 128 bits, on any vector.
/// `Smin`/`Smax`/`Umin`/`Umax` are the lane-wise signed / unsigned minimum
/// and maximum of integer vectors (NEON has them for the 8/16/32-bit lanes;
/// a `V2I64` form is the compare + select pair).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum VecBinOp {
    Add,
    Sub,
    Mul,
    And,
    Or,
    Xor,
    Smin,
    Smax,
    Umin,
    Umax,
}

/// Lane-wise shift of an integer vector by an immediate count (below the
/// lane width; the verifier checks).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum VecShiftOp {
    Shl,
    /// Logical (zero-filling) right shift.
    Ushr,
    /// Arithmetic (sign-filling) right shift.
    Sshr,
}

/// One instruction. Its result, if any, is `Instruction::result`.
#[derive(Debug, Eq, PartialEq, Hash)]
pub enum Op {
    /// A constant of the given type. Integers and pointers are stored in the
    /// low bits of `bits` and sign-/zero-extension is irrelevant because the
    /// type fixes the width. Floats are stored as their IEEE bit pattern.
    /// `Bool` is 0 or 1.
    Const { ty: Type, bits: u64 },

    IntBin { op: IntBinOp, a: Value, b: Value },
    IntOvf { op: IntOvfOp, a: Value, b: Value },
    IntUn { op: IntUnOp, a: Value },
    Icmp { cond: IntCond, a: Value, b: Value },

    FloatBin { op: FloatBinOp, a: Value, b: Value },
    FloatUn { op: FloatUnOp, a: Value },
    Fcmp { cond: FloatCond, a: Value, b: Value },

    Conv { op: ConvOp, to: Type, a: Value },

    /// `cond ? a : b`, both operands already evaluated. Same type.
    Select { cond: Value, a: Value, b: Value },

    /// Load a `ty` from `ptr + offset`. The address must be aligned to
    /// `ty.bytes()` unless `unaligned` is set.
    Load { ty: Type, ptr: Value, offset: i32, unaligned: bool },
    /// Load a narrow integer (`I8`/`I16`/`I32`) from `ptr + offset` and
    /// zero-extend it to `I64`. Same alignment rule as `Load`. Exists because
    /// every heap layout field (line pointers, header words, varlena
    /// headers) is unsigned; backends emit one zero-extending load.
    LoadZext { ty: Type, ptr: Value, offset: i32, unaligned: bool },
    /// Store `value` to `ptr + offset`. Same alignment rule as `Load`.
    Store { ptr: Value, offset: i32, value: Value, unaligned: bool },

    /// Address of a stack slot declared by the enclosing function. Slots are
    /// live for the whole function invocation.
    StackAddr { slot: StackSlot },

    /// Pointer arithmetic: `base + index * scale + offset`. `index` is `I64`.
    /// Exists so backends can use scaled addressing; equivalent to
    /// `IntToPtr(PtrToInt(base) + index*scale + offset)`.
    PtrAdd { base: Value, index: Value, scale: u8, offset: i32 },

    /// Call a function in this program. Arguments match the callee's
    /// parameter types; the result, if any, matches its return type.
    Call { callee: Func, args: IrSlice<Value> },

    /// Call a host (Rust `extern "C"`) function. See `HostFnDecl`.
    CallHost { callee: HostFn, args: IrSlice<Value> },

    /// Prefetch the cache line at `ptr + offset` for a read into L1
    /// (AArch64 `prfm pldl1keep`, x86-64 `prefetcht0`). A hint with no
    /// result and no semantics: it never faults, the interpreter ignores
    /// it, and a backend may drop it. The address need not be valid.
    Prefetch { ptr: Value, offset: i32 },
    /// CRC-32C (Castagnoli, reflected polynomial 0x82F63B78) of the eight
    /// bytes of `data` (`I64`, little-endian) folded into the 32-bit
    /// accumulator `acc` (`I64`; only its low 32 bits are read): the
    /// result is the new 32-bit CRC zero-extended to `I64`. One
    /// instruction on AArch64 (`crc32cx`) and x86-64 (`crc32 r64`).
    Crc32c { acc: Value, data: Value },
    /// The high word of the 128-bit sum `ahi:alo + bhi:blo` (all `I64`): `ahi + bhi + carry(alo + blo)`, the low word being a
    /// plain `iadd`. `cmn` + `adc` on AArch64, `add` + `adc` on x86-64.
    AddHi { alo: Value, blo: Value, ahi: Value, bhi: Value },

    // -- fixed 128-bit vectors (docs/design/gravity-simple/simd.md) ------
    // The op set is what the columnar lowerer's vector loops use (a
    // filter mask over lanes, masked accumulators, the reductions at the
    // granule end); it is not a general vector ISA. The interpreter
    // executes every op lane by lane; the backends map each to one NEON
    // instruction or a short fixed sequence.
    /// Load a 128-bit vector of type `ty` from `ptr + offset`; aligned to
    /// 16 unless `unaligned`.
    VecLoad { ty: Type, ptr: Value, offset: i32, unaligned: bool },
    /// Store a vector to `ptr + offset`. Same alignment rule.
    VecStore { ptr: Value, offset: i32, value: Value, unaligned: bool },
    /// Lane-wise binary op; see `VecBinOp`.
    VecBin { op: VecBinOp, a: Value, b: Value },
    /// Lane-wise shift by an immediate; see `VecShiftOp`.
    VecShift { op: VecShiftOp, a: Value, count: u8 },
    /// Lane-wise integer compare producing a mask vector of the operands'
    /// type: a lane is all ones where the predicate holds, else zero.
    VecCmp { cond: IntCond, a: Value, b: Value },
    /// Bitwise select: `(mask & a) | (!mask & b)`, all three of one vector
    /// type. With a `VecCmp` mask this is the lane-wise `mask ? a : b`.
    VecSelect { mask: Value, a: Value, b: Value },
    /// Horizontal add of the lanes. Integer lanes are sign-extended and
    /// summed as `I64` (wrapping): `V2I64 -> I64` is the exact sum, the
    /// narrower vectors widen so a count or sum of narrow lanes needs no
    /// per-lane widening first. `V2F64 -> F64` is `lane0 + lane1`;
    /// `V4F32 -> F32` is `(lane0 + lane1) + (lane2 + lane3)` (the pairwise
    /// order of `faddp`, which the backends emit).
    VecReduceAdd { a: Value },
    /// The sign bits of the lanes as an `I64` bitmask, bit `i` = lane `i`'s
    /// most significant bit (a `VecCmp` mask becomes one bit per lane).
    VecMovmask { a: Value },
    /// Every lane of `ty` set to the scalar `a` (whose type is `ty.lane()`).
    VecSplat { ty: Type, a: Value },
    /// Lane `lane` of `a` as a scalar of the lane type (canonical: narrow
    /// integers sign-extended).
    VecExtract { a: Value, lane: u8 },
    /// `a` with lane `lane` replaced by the scalar `value`.
    VecInsert { a: Value, lane: u8, value: Value },
    /// The low (`high == false`) or high half of `a`'s integer lanes, each
    /// extended (`signed` or zero) to twice its width: `V16I8 -> V8I16`,
    /// `V8I16 -> V4I32`, `V4I32 -> V2I64`.
    VecWiden { a: Value, signed: bool, high: bool },
    /// Both operands' integer lanes truncated to half their width, `a`'s
    /// in the low half of the result and `b`'s in the high half:
    /// `V2I64 x2 -> V4I32`, `V4I32 x2 -> V8I16`, `V8I16 x2 -> V16I8`.
    VecNarrow { a: Value, b: Value },
}

/// An instruction: an operation plus the value it defines, if any.
/// `result` is `Some` iff the operation produces a value (`Store` and calls
/// to void functions produce none).
#[derive(Debug, Eq, PartialEq, Hash)]
pub struct Instruction {
    pub result: Option<Value>,
    pub op: Op,
}

// ----------------------------------------------------------------------------
// Control flow
// ----------------------------------------------------------------------------

/// A control-flow edge: the target block and the arguments bound to its
/// parameters. Arguments are transferred simultaneously (parallel copy).
#[derive(Debug, Eq, PartialEq, Hash)]
pub struct Edge {
    pub target: Block,
    pub args: IrSlice<Value>,
}

/// One `Switch` case: an exact integer bit pattern of the scrutinee's type.
#[derive(Debug, Eq, PartialEq, Hash)]
pub struct SwitchCase {
    pub value: u64,
    pub edge: Edge,
}
impl Op {
    /// A copy of an op that holds no list (every op but a call); `None` for a call (its copy is `CloneIn`'s, in a program).
    pub fn copy_flat(&self) -> Option<Op> {
        Some(match *self {
            Op::Const { ty, bits } => Op::Const { ty, bits },
            Op::IntBin { op, a, b } => Op::IntBin { op, a, b },
            Op::IntOvf { op, a, b } => Op::IntOvf { op, a, b },
            Op::IntUn { op, a } => Op::IntUn { op, a },
            Op::Icmp { cond, a, b } => Op::Icmp { cond, a, b },
            Op::FloatBin { op, a, b } => Op::FloatBin { op, a, b },
            Op::FloatUn { op, a } => Op::FloatUn { op, a },
            Op::Fcmp { cond, a, b } => Op::Fcmp { cond, a, b },
            Op::Conv { op, to, a } => Op::Conv { op, to, a },
            Op::Select { cond, a, b } => Op::Select { cond, a, b },
            Op::Load { ty, ptr, offset, unaligned } => Op::Load { ty, ptr, offset, unaligned },
            Op::LoadZext { ty, ptr, offset, unaligned } => Op::LoadZext { ty, ptr, offset, unaligned },
            Op::Store { ptr, offset, value, unaligned } => Op::Store { ptr, offset, value, unaligned },
            Op::StackAddr { slot } => Op::StackAddr { slot },
            Op::PtrAdd { base, index, scale, offset } => Op::PtrAdd { base, index, scale, offset },
            Op::Call { .. } | Op::CallHost { .. } => return None,
            Op::Prefetch { ptr, offset } => Op::Prefetch { ptr, offset },
            Op::Crc32c { acc, data } => Op::Crc32c { acc, data },
            Op::AddHi { alo, blo, ahi, bhi } => Op::AddHi { alo, blo, ahi, bhi },
            Op::VecLoad { ty, ptr, offset, unaligned } => Op::VecLoad { ty, ptr, offset, unaligned },
            Op::VecStore { ptr, offset, value, unaligned } => Op::VecStore { ptr, offset, value, unaligned },
            Op::VecBin { op, a, b } => Op::VecBin { op, a, b },
            Op::VecShift { op, a, count } => Op::VecShift { op, a, count },
            Op::VecCmp { cond, a, b } => Op::VecCmp { cond, a, b },
            Op::VecSelect { mask, a, b } => Op::VecSelect { mask, a, b },
            Op::VecReduceAdd { a } => Op::VecReduceAdd { a },
            Op::VecMovmask { a } => Op::VecMovmask { a },
            Op::VecSplat { ty, a } => Op::VecSplat { ty, a },
            Op::VecExtract { a, lane } => Op::VecExtract { a, lane },
            Op::VecInsert { a, lane, value } => Op::VecInsert { a, lane, value },
            Op::VecWiden { a, signed, high } => Op::VecWiden { a, signed, high },
            Op::VecNarrow { a, b } => Op::VecNarrow { a, b },
        })
    }

    /// The values this instruction reads, in operand order, appended to `out`: no allocation once `out` has room (the
    /// verifier's walk over every use of every instruction).
    pub fn operands_into(&self, out: &mut Vec<Value>) {
        let mut buf = [Value(0); 4];
        out.extend_from_slice(self.operand_list(&mut buf));
    }

    /// Each value this instruction reads, in operand order (no allocation).
    pub fn for_each_operand(&self, mut f: impl FnMut(Value)) {
        let mut buf = [Value(0); 4];
        self.operand_list(&mut buf).iter().for_each(|&v| f(v));
    }

    /// The operands: a call's argument list, else at most four copied into `buf`.
    fn operand_list<'a>(&'a self, buf: &'a mut [Value; 4]) -> &'a [Value] {
        let mut put = |vs: &[Value]| {
            buf[..vs.len()].copy_from_slice(vs);
            vs.len()
        };
        let n = match self {
            Op::Const { .. } | Op::StackAddr { .. } => 0,
            Op::IntBin { a, b, .. } | Op::IntOvf { a, b, .. } | Op::Icmp { a, b, .. } | Op::FloatBin { a, b, .. } | Op::Fcmp { a, b, .. } => put(&[*a, *b]),
            Op::IntUn { a, .. } | Op::FloatUn { a, .. } | Op::Conv { a, .. } => put(&[*a]),
            Op::Select { cond, a, b } => put(&[*cond, *a, *b]),
            Op::Load { ptr, .. } | Op::LoadZext { ptr, .. } => put(&[*ptr]),
            Op::Store { ptr, value, .. } => put(&[*ptr, *value]),
            Op::PtrAdd { base, index, .. } => put(&[*base, *index]),
            Op::Prefetch { ptr, .. } => put(&[*ptr]),
            Op::Crc32c { acc, data } => put(&[*acc, *data]),
            Op::AddHi { alo, blo, ahi, bhi } => put(&[*alo, *blo, *ahi, *bhi]),
            Op::Call { args, .. } | Op::CallHost { args, .. } => return args,
            Op::VecLoad { ptr, .. } => put(&[*ptr]),
            Op::VecStore { ptr, value, .. } => put(&[*ptr, *value]),
            Op::VecBin { a, b, .. } | Op::VecCmp { a, b, .. } | Op::VecNarrow { a, b } => put(&[*a, *b]),
            Op::VecShift { a, .. } | Op::VecReduceAdd { a } | Op::VecMovmask { a } | Op::VecSplat { a, .. } | Op::VecExtract { a, .. } | Op::VecWiden { a, .. } => put(&[*a]),
            Op::VecSelect { mask, a, b } => put(&[*mask, *a, *b]),
            Op::VecInsert { a, value, .. } => put(&[*a, *value]),
        };
        &buf[..n]
    }
}

impl Terminator {
    /// The values the terminator reads besides its edges' arguments, appended to `out`.
    pub fn operands_into(&self, out: &mut Vec<Value>) {
        self.for_each_operand(|v| out.push(v));
    }

    /// Each value the terminator reads besides its edges' arguments (no allocation).
    pub fn for_each_operand(&self, mut f: impl FnMut(Value)) {
        match self {
            Terminator::Br(_) | Terminator::Unreachable => {}
            Terminator::CondBr { cond, .. } => f(*cond),
            Terminator::Switch { value, .. } => f(*value),
            Terminator::Return(vs) => vs.iter().for_each(|&v| f(v)),
        }
    }
}


/// Block terminators. Every block has exactly one.
#[derive(Debug, Eq, PartialEq, Hash)]
pub enum Terminator {
    Br(Edge),
    /// `cond` is `Bool`. Both edges may target the same block.
    CondBr { cond: Value, then_edge: Edge, else_edge: Edge },
    /// Integer multi-way branch. Cases are distinct; `default` is mandatory.
    Switch { value: Value, cases: IrSlice<SwitchCase>, default: Edge },
    /// Return from the function. `values` matches the function's return types.
    Return(IrSlice<Value>),
    /// Control must never reach here. Backends may emit a trap.
    Unreachable,
}

impl Terminator {
    /// All outgoing edges, in a fixed order.
    pub fn edges(&self) -> impl Iterator<Item = &Edge> {
        let (a, b, rest, d): (Option<&Edge>, Option<&Edge>, &[SwitchCase], Option<&Edge>) = match self {
            Terminator::Br(e) => (Some(e), None, &[], None),
            Terminator::CondBr { then_edge, else_edge, .. } => (Some(then_edge), Some(else_edge), &[], None),
            Terminator::Switch { cases, default, .. } => (None, None, &cases[..], Some(default)),
            Terminator::Return(_) | Terminator::Unreachable => (None, None, &[], None),
        };
        a.into_iter().chain(b).chain(rest.iter().map(|c| &c.edge)).chain(d)
    }
}

/// A basic block: typed parameters, straight-line instructions, one terminator.
#[derive(Debug, Eq, PartialEq, Hash)]
pub struct BasicBlock {
    /// Values defined on entry, bound from the incoming edge's arguments.
    pub params: IrSlice<Value>,
    pub insts: IrVec<Instruction>,
    pub term: Terminator,
}

// ----------------------------------------------------------------------------
// Functions and programs
// ----------------------------------------------------------------------------

/// A fixed-size, fixed-alignment region of the function's stack frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct StackSlotDecl {
    pub bytes: u32,
    pub align: u32,
}

/// A function in the program. Parameters are the entry block's parameters.
#[derive(Debug, Eq, PartialEq, Hash)]
pub struct Function {
    pub name: IrStr,
    pub params: IrVec<Type>,
    pub returns: IrVec<Type>,
    /// Type of every `Value` in this function, indexed by `Value::index`.
    pub values: IrVec<Type>,
    pub stack_slots: IrVec<StackSlotDecl>,
    pub blocks: IrVec<BasicBlock>,
    pub entry: Block,
    /// Optimization hints from the lowerer. Never affect semantics; the
    /// interpreter ignores them, backends may.
    pub hints: Hints,
}

/// What the lowerer knows and a single-pass backend cannot recover: expected
/// execution frequencies, which blocks and calls are cold, loop structure,
/// which values can be recomputed instead of spilled, and which narrow
/// integer results are known to fit their type.
#[derive(Debug, PartialEq)]
pub struct Hints {
    /// Blocks executed rarely (error raises, slow host paths). A backend
    /// lays them out last and does not optimize branches into them; a call
    /// in a cold block is a cold call.
    pub cold_blocks: IrVec<Block>,
    /// Expected executions of a block per function call, relative to the
    /// entry (1.0). Absent blocks inherit their loop's frequency.
    pub block_freq: IrVec<(Block, f32)>,
    /// Loop headers, explicitly; their parameters are the loop-carried state.
    pub loop_headers: IrVec<Block>,
    /// For a block ending in `CondBr`: whether the `then` edge is the likely
    /// one (`true`) or the `else` edge (`false`).
    pub likely: IrVec<(Block, bool)>,
    /// Values a backend may recompute from their operands (a load from
    /// memory that is invariant for the call) instead of spilling.
    pub remat: IrVec<Value>,
    /// Results of narrow-integer arithmetic that are known to fit their
    /// type: no canonicalization (sign extension) needed after the op.
    pub nowrap: IrVec<Value>,
}

// `block_freq` carries `f32`s: equality and hashing go by bit pattern, so a
// program is `Eq` / `Hash` as a whole (the program cache's content key).
impl Eq for Hints {}

impl Hints {
    /// No hint at all (what a function declared and never hinted has).
    pub fn is_empty(&self) -> bool {
        self.cold_blocks.is_empty() && self.block_freq.is_empty() && self.loop_headers.is_empty() && self.likely.is_empty() && self.remat.is_empty() && self.nowrap.is_empty()
    }
}

impl core::hash::Hash for Hints {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.cold_blocks.hash(state);
        self.block_freq.len().hash(state);
        for (b, f) in &self.block_freq {
            b.hash(state);
            f.to_bits().hash(state);
        }
        self.loop_headers.hash(state);
        self.likely.hash(state);
        self.remat.hash(state);
        self.nowrap.hash(state);
    }
}

impl Function {
    /// The right to make vectors in this function's program (its root): `&mut`, so nothing else of the program is in use.
    pub fn mint(&mut self) -> Mint {
        Mint(raw::again(self.values.allocator()))
    }

    pub fn value_type(&self, v: Value) -> Type {
        self.values[v.index()]
    }
    pub fn block(&self, b: Block) -> &BasicBlock {
        &self.blocks[b.index()]
    }
}

/// The classes of executor state cells a host may read or write
/// (`docs/design/gravity-simple/register-state.md` §1.1). A cell the
/// lowerer carries in registers (`promote`) is written back before a
/// host whose mask names its class and reloaded after it; a host whose
/// mask is `NONE` is transparent to the carried state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct StateMask(pub u8);

impl StateMask {
    pub const NONE: StateMask = StateMask(0);
    /// The output count and the source cursors (`OUT_COUNT`, `CURSOR`,
    /// `IN_CURSOR`).
    pub const OUTPUT: StateMask = StateMask(1);
    /// The per-op state words (`OPSTATE`).
    pub const OPSTATE: StateMask = StateMask(2);
    /// The aggregate records (the fused PlainAgg record, the sorted-agg
    /// region).
    pub const AGG: StateMask = StateMask(4);
    /// The set loop's pending word (`PS_PENDING`).
    pub const PS: StateMask = StateMask(8);
    pub const ALL: StateMask = StateMask(0xff);
    pub const fn contains(self, other: StateMask) -> bool {
        self.0 & other.0 != 0
    }
    pub const fn or(self, other: StateMask) -> StateMask {
        StateMask(self.0 | other.0)
    }
}

impl Default for StateMask {
    /// An undeclared host may touch everything (the safe default).
    fn default() -> Self {
        StateMask::ALL
    }
}

/// How a host function behaves, for the backend and for optimization.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Default)]
pub struct HostFlags {
    /// The call can raise a SQL error. The lowerer follows the host-call error
    /// convention (see crate docs of the lowerer); the IR itself does not
    /// define how errors propagate.
    pub may_error: bool,
    /// No side effects and result depends only on arguments. A pure call whose
    /// result is unused may be deleted; two identical pure calls may be merged.
    pub pure: bool,
    /// The call may read or write memory reachable from its pointer arguments
    /// or from global executor state. Cleared only for calls that touch
    /// nothing but their scalar arguments.
    pub touches_memory: bool,
    /// The host may be entered from a worker thread of the parallel driver
    /// (`docs/design/gravity-simple/parallel-driver.md` §3): it is pure over
    /// its arguments or writes only into worker-owned memory reached from
    /// the worker's host environment, and touches no backend-global state.
    /// A program is worker-eligible only if every host it calls has it.
    pub worker: bool,
    /// The executor state cells the call may read or write; see
    /// `StateMask` (default: all of them).
    pub state: StateMask,
}

/// The kernels' poll host (D3406, lowerv4 `frame::cancel::check`): a cold
/// block that calls it is the set-byte detour of a cancel check. The TPDE
/// layout sinks that block to the end of its loop (D3651), so the check's
/// clear-byte path falls through as it did before the poll existed.
pub const KERNEL_POLL: &str = "kernel_poll";

/// A host function reachable from generated code. The lowerer binds the
/// address in-process; there is no symbol resolution at runtime.
#[derive(Debug, Eq, PartialEq, Hash)]
pub struct HostFnDecl {
    /// For printing and diagnostics, and one layout rule: `KERNEL_POLL`.
    pub name: IrStr,
    pub params: IrVec<Type>,
    /// `None` for a void function.
    pub ret: Option<Type>,
    /// Address of an `extern "C"` function with exactly this signature.
    pub addr: usize,
    pub flags: HostFlags,
}

/// A compilation unit: functions plus the host functions they call. Every vector in it lives in its own root context
/// (`root`, an `mcx::SendRoot`: P3), named at its birth for the memory ledger's owner (`new_owned`: a lowering's
/// statement, the program caches' for tier 0's copy), so its bytes are that owner's by construction. A vector of a
/// program stays in that program: made by its `Mint` (or a `CloneIn` into it), never moved into another or kept past it.
/// The fields drop in order, every vector before the root.
pub struct Program {
    pub funcs: IrVec<Function>,
    pub host_fns: IrVec<HostFnDecl>,
    root: SendRoot,
    owner: Option<memledger::Owner>,
}

impl Program {
    /// An empty program in a new root, its bytes on the creating thread's owner (an unnamed root's) until `set_owner`.
    pub fn new() -> Program {
        Program::in_root(SendRoot::new(ROOT_NAME), None)
    }

    /// An empty program in a new root named `owner`'s at its birth.
    pub fn new_owned(owner: memledger::Owner) -> Program {
        Program::in_root(SendRoot::new_owned(ROOT_NAME, owner), Some(owner))
    }

    fn in_root(mut root: SendRoot, owner: Option<memledger::Owner>) -> Program {
        let (funcs, host_fns) = (IrVec::new_in(raw::handle(&mut root)), IrVec::new_in(raw::handle(&mut root)));
        Program { funcs, host_fns, root, owner }
    }

    /// Its bytes on `owner` from now (a holder adopting it); they move in one step.
    pub fn set_owner(&mut self, owner: memledger::Owner) {
        self.root.set_owner(owner);
        self.owner = Some(owner);
    }

    /// The owner named at its birth or by `set_owner`; `None`: the creating thread's.
    pub fn owner(&self) -> Option<memledger::Owner> {
        self.owner
    }

    /// Bytes of its root's blocks (what its owner holds for it).
    pub fn bytes(&self) -> usize {
        self.root.allocated()
    }

    /// The right to make vectors in this program: `&mut`, so the program is not shared while they are made.
    pub fn mint(&mut self) -> Mint {
        Mint(raw::handle(&mut self.root))
    }

    /// A deep copy in a new root of its own, named `owner`'s (`None`: the creating thread's) at its birth: what tier 0
    /// hands the compile pool (backend `tiered_codes`).
    pub fn clone_owned(&self, owner: Option<memledger::Owner>) -> Program {
        let mut p = match owner {
            Some(o) => Program::new_owned(o),
            None => Program::new(),
        };
        let m = p.mint();
        p.funcs = clone_vec(&self.funcs, &m);
        p.host_fns = clone_vec(&self.host_fns, &m);
        p
    }

    /// Function `f` copied as a new function of this program (a template to rewrite: lowerv4 `twin`).
    pub fn dup_func(&mut self, f: Func) -> Function {
        let m = self.mint();
        self.funcs[f.index()].clone_in(&m)
    }

    /// D3168 S0: function `f` alone as a program of its own (its entry `Func(0)`), with every host function at its index,
    /// named `owner`'s at its birth; `None` when `f` calls a function of this program (that callee's index would not hold).
    pub fn leaf(&self, f: Func, owner: Option<memledger::Owner>) -> Option<Program> {
        let src = &self.funcs[f.index()];
        if src.blocks.iter().any(|b| b.insts.iter().any(|i| matches!(i.op, Op::Call { .. }))) {
            return None;
        }
        let mut p = match owner {
            Some(o) => Program::new_owned(o),
            None => Program::new(),
        };
        let m = p.mint();
        let mut funcs = m.vec_with_capacity(1);
        funcs.push(src.clone_in(&m));
        p.funcs = funcs;
        p.host_fns = clone_vec(&self.host_fns, &m);
        Some(p)
    }

    pub fn func(&self, f: Func) -> &Function {
        &self.funcs[f.index()]
    }
    pub fn host_fn(&self, h: HostFn) -> &HostFnDecl {
        &self.host_fns[h.index()]
    }
}

const ROOT_NAME: &str = "GravityIR";

impl Default for Program {
    fn default() -> Program {
        Program::new()
    }
}

impl Clone for Program {
    /// A deep copy in a new root, owned as this program is.
    fn clone(&self) -> Program {
        self.clone_owned(self.owner)
    }
}

impl PartialEq for Program {
    fn eq(&self, o: &Program) -> bool {
        self.funcs == o.funcs && self.host_fns == o.host_fns
    }
}

impl Eq for Program {}

// The program cache's content key: the functions, then the hosts, as the derived hash of the two vectors was.
impl core::hash::Hash for Program {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.funcs.hash(state);
        self.host_fns.hash(state);
    }
}

impl core::fmt::Debug for Program {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Program").field("funcs", &self.funcs).field("host_fns", &self.host_fns).finish()
    }
}

/// The right to make vectors in one program's root (`Program::mint`, `Function::mint`, taken under `&mut`). What it
/// makes belongs in that program.
pub struct Mint(IrA);

impl Mint {
    fn a(&self) -> IrA {
        raw::again(&self.0)
    }

    pub fn vec<T>(&self) -> IrVec<T> {
        IrVec::new_in(self.a())
    }

    pub fn vec_with_capacity<T>(&self, n: usize) -> IrVec<T> {
        IrVec::with_capacity_in(n, self.a())
    }

    /// `vec![x; n]` in the root.
    pub fn vec_filled<T: Clone>(&self, x: T, n: usize) -> IrVec<T> {
        let mut v = IrVec::with_capacity_in(n, self.a());
        v.resize(n, x);
        v
    }

    pub fn vec_from<T: Clone>(&self, s: &[T]) -> IrVec<T> {
        let mut v = IrVec::with_capacity_in(s.len(), self.a());
        v.extend_from_slice(s);
        v
    }

    pub fn collect<T>(&self, it: impl IntoIterator<Item = T>) -> IrVec<T> {
        let it = it.into_iter();
        let mut v = IrVec::with_capacity_in(it.size_hint().0, self.a());
        for x in it {
            v.push(x);
        }
        v
    }

    pub fn slice_from<T: Clone>(&self, s: &[T]) -> IrSlice<T> {
        IrSlice::from_vec(self.vec_from(s))
    }

    pub fn slice_collect<T>(&self, it: impl IntoIterator<Item = T>) -> IrSlice<T> {
        IrSlice::from_vec(self.collect(it))
    }

    /// `s` and then `x`, a new list (a fixed list grows by a copy: its growers are rare passes).
    pub fn appended<T: Clone>(&self, s: &[T], x: T) -> IrSlice<T> {
        let mut v = self.vec_with_capacity(s.len() + 1);
        v.extend_from_slice(s);
        v.push(x);
        IrSlice::from_vec(v)
    }

    pub fn str(&self, s: &str) -> IrStr {
        IrStr(self.slice_from(s.as_bytes()))
    }

    pub fn edge(&self, target: Block, args: &[Value]) -> Edge {
        Edge { target, args: self.slice_from(args) }
    }

    /// A function with no body yet.
    pub fn function(&self, name: &str, params: &[Type], returns: &[Type]) -> Function {
        let (values, stack_slots, blocks) = (self.vec(), self.vec(), self.vec());
        Function { name: self.str(name), params: self.vec_from(params), returns: self.vec_from(returns), values, stack_slots, blocks, entry: Block(0), hints: self.hints() }
    }

    pub fn hints(&self) -> Hints {
        Hints { cold_blocks: self.vec(), block_freq: self.vec(), loop_headers: self.vec(), likely: self.vec(), remat: self.vec(), nowrap: self.vec() }
    }
}

/// A copy of an IR value made in the program a `Mint` names (IR values are not `Clone`: a copy is always somewhere).
pub trait CloneIn {
    fn clone_in(&self, m: &Mint) -> Self;
}

fn clone_vec<T: CloneIn>(v: &IrVec<T>, m: &Mint) -> IrVec<T> {
    let mut out = m.vec_with_capacity(v.len());
    for x in v.iter() {
        out.push(x.clone_in(m));
    }
    out
}

impl CloneIn for Function {
    fn clone_in(&self, m: &Mint) -> Function {
        Function {
            name: self.name.clone_in(m),
            params: m.vec_from(&self.params),
            returns: m.vec_from(&self.returns),
            values: m.vec_from(&self.values),
            stack_slots: m.vec_from(&self.stack_slots),
            blocks: clone_vec(&self.blocks, m),
            entry: self.entry,
            hints: self.hints.clone_in(m),
        }
    }
}

impl CloneIn for BasicBlock {
    fn clone_in(&self, m: &Mint) -> BasicBlock {
        BasicBlock { params: m.slice_from(&self.params), insts: clone_vec(&self.insts, m), term: self.term.clone_in(m) }
    }
}

impl CloneIn for Instruction {
    fn clone_in(&self, m: &Mint) -> Instruction {
        Instruction { result: self.result, op: self.op.clone_in(m) }
    }
}

impl CloneIn for Op {
    fn clone_in(&self, m: &Mint) -> Op {
        match self {
            Op::Call { callee, args } => Op::Call { callee: *callee, args: m.slice_from(args) },
            Op::CallHost { callee, args } => Op::CallHost { callee: *callee, args: m.slice_from(args) },
            op => op.copy_flat().expect("an op with no list"),
        }
    }
}

impl CloneIn for Edge {
    fn clone_in(&self, m: &Mint) -> Edge {
        m.edge(self.target, &self.args)
    }
}

impl CloneIn for SwitchCase {
    fn clone_in(&self, m: &Mint) -> SwitchCase {
        SwitchCase { value: self.value, edge: self.edge.clone_in(m) }
    }
}

impl CloneIn for Terminator {
    fn clone_in(&self, m: &Mint) -> Terminator {
        match self {
            Terminator::Br(e) => Terminator::Br(e.clone_in(m)),
            Terminator::CondBr { cond, then_edge, else_edge } => Terminator::CondBr { cond: *cond, then_edge: then_edge.clone_in(m), else_edge: else_edge.clone_in(m) },
            Terminator::Switch { value, cases, default } => Terminator::Switch { value: *value, cases: m.slice_collect(cases.iter().map(|c| c.clone_in(m))), default: default.clone_in(m) },
            Terminator::Return(vs) => Terminator::Return(m.slice_from(vs)),
            Terminator::Unreachable => Terminator::Unreachable,
        }
    }
}

impl CloneIn for Hints {
    fn clone_in(&self, m: &Mint) -> Hints {
        Hints {
            cold_blocks: m.vec_from(&self.cold_blocks),
            block_freq: m.vec_from(&self.block_freq),
            loop_headers: m.vec_from(&self.loop_headers),
            likely: m.vec_from(&self.likely),
            remat: m.vec_from(&self.remat),
            nowrap: m.vec_from(&self.nowrap),
        }
    }
}

impl CloneIn for HostFnDecl {
    fn clone_in(&self, m: &Mint) -> HostFnDecl {
        HostFnDecl { name: self.name.clone_in(m), params: m.vec_from(&self.params), ret: self.ret, addr: self.addr, flags: self.flags }
    }
}

impl CloneIn for IrStr {
    fn clone_in(&self, m: &Mint) -> IrStr {
        IrStr(m.slice_from(&self.0))
    }
}

/// A name in a program's root (`Function::name`, `HostFnDecl::name`): compared, hashed, printed as its `str`.
pub struct IrStr(IrSlice<u8>);

impl IrStr {
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.0).expect("an IR name is UTF-8 (`Mint::str`)")
    }
}

impl core::ops::Deref for IrStr {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq for IrStr {
    fn eq(&self, o: &IrStr) -> bool {
        self.0[..] == o.0[..]
    }
}

impl Eq for IrStr {}

impl PartialEq<str> for IrStr {
    fn eq(&self, o: &str) -> bool {
        self.0[..] == *o.as_bytes()
    }
}

impl PartialEq<&str> for IrStr {
    fn eq(&self, o: &&str) -> bool {
        self.0[..] == *o.as_bytes()
    }
}

// `str`'s hash (its bytes, then 0xff): the content key a `String` name gave.
impl core::hash::Hash for IrStr {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        state.write(&self.0);
        state.write_u8(0xff);
    }
}

impl core::fmt::Debug for IrStr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl core::fmt::Display for IrStr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub mod builder;
pub mod jit_tags;
pub use jit_tags::{JitRun, JitTags};
pub mod promote;
pub mod prune;
pub mod text;
pub mod verify;
