//! Local forwarding for a private aggregate record's validity byte. These bytes are not synchronization flags.
//! Keep every store; reuse a known byte only within the current block, before any call or possibly aliasing write.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, Op, Type, Value};

// Bound lowering work and the distance over which a forwarded value stays live. No cache or per-row metadata.
const LOOKBACK: usize = 16;

pub(super) fn load(b: &mut FunctionBuilder<'_>, base: Value, off: i32) -> Value {
    let mut found = None;
    for inst in b.func().blocks[b.current().index()].insts.iter().rev().take(LOOKBACK) {
        match inst.op {
            Op::LoadZext { ty: Type::I8, ptr, offset, .. } if ptr == base && offset == off => {
                found = inst.result;
                break;
            }
            Op::Load { ty: Type::I8, ptr, offset, .. } if ptr == base && offset == off => {
                found = inst.result;
                break;
            }
            Op::Store { ptr, offset, value, .. } => {
                if ptr == base && offset == off && b.value_type(value) == Type::I8 {
                    found = Some(value);
                    break;
                }
                // Distinct SSA pointers may alias. Only an interval disjoint from this byte, relative to the SAME
                // pointer, is known safe. Widen before adding the width so extreme signed offsets cannot overflow.
                let end = i64::from(offset) + i64::from(b.value_type(value).bytes());
                if ptr != base || (i64::from(off) >= i64::from(offset) && i64::from(off) < end) {
                    break;
                }
            }
            Op::Const { .. } | Op::IntBin { .. } | Op::IntOvf { .. } | Op::IntUn { .. } | Op::Icmp { .. }
            | Op::FloatBin { .. } | Op::FloatUn { .. } | Op::Fcmp { .. } | Op::Conv { .. } | Op::Select { .. }
            | Op::Load { .. } | Op::LoadZext { .. } | Op::StackAddr { .. } | Op::PtrAdd { .. }
            | Op::Prefetch { .. } | Op::Crc32c { .. } | Op::AddHi { .. } => {}
            // Includes all host/program calls, vector writes, and any future instruction not explicitly reviewed.
            _ => break,
        }
    }
    match found {
        Some(v) if b.value_type(v) == Type::I8 => b.conv(ConvOp::Zext, Type::I64, v),
        Some(v) => v,
        None => b.load_zext(Type::I8, base, off),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_ir::{Func, IntBinOp, Program};

    fn build(f: impl FnOnce(&mut FunctionBuilder<'_>, &[Value]) -> Value) -> (Program, Func) {
        let mut pb = ProgramBuilder::new();
        let fid = pb.declare_func("byte", &[Type::Ptr, Type::Ptr, Type::I64], &[Type::I64]);
        let mut b = pb.define(fid);
        let p = b.params().to_vec();
        let v = f(&mut b, &p);
        b.ret(&[v]);
        let p = pb.finish();
        gravity_ir::verify::verify(&p).unwrap();
        (p, fid)
    }

    fn run(p: &Program, f: Func, input: u64) -> u64 {
        let mut words = [0u64; 4];
        let ptr = words.as_mut_ptr() as u64;
        // The two distinct SSA parameters deliberately alias at runtime.
        gravity_interp::Interp::new(p).run(f, &[ptr, ptr, input]).unwrap()[0]
    }

    fn reads(p: &Program, f: Func) -> usize {
        p.funcs[f.index()].blocks.iter().flat_map(|b| &b.insts)
            .filter(|i| matches!(i.op, Op::Load { ty: Type::I8, .. } | Op::LoadZext { ty: Type::I8, .. })).count()
    }

    #[test]
    fn stored_bytes_remain_unsigned_across_disjoint_aggregate_writes() {
        let (p, f) = build(|b, p| {
            let v = b.conv(ConvOp::Trunc, Type::I8, p[2]);
            b.store(p[0], 8, v);
            b.store(p[0], 0, p[2]); // adjacent count/sum word, ending immediately before the byte
            load(b, p[0], 8)
        });
        assert_eq!(reads(&p, f), 0);
        for byte in 0..=255 {
            assert_eq!(run(&p, f, byte | 0xdead_0000), byte);
        }
    }

    #[test]
    fn loaded_bytes_can_be_reused_without_sign_extension() {
        for narrow in [false, true] {
            let (p, f) = build(|b, p| {
                b.store(p[1], 0, p[2]);
                let _first = if narrow { b.load(Type::I8, p[0], 0) } else { b.load_zext(Type::I8, p[0], 0) };
                load(b, p[0], 0)
            });
            assert_eq!(reads(&p, f), 1);
            for byte in 0..=255 {
                assert_eq!(run(&p, f, byte), byte);
            }
        }
    }

    #[test]
    fn aliasing_and_overlapping_writes_invalidate_the_known_byte() {
        for alias in [false, true] {
            let (p, f) = build(|b, p| {
                let old = b.iconst(Type::I8, 17);
                b.store(p[0], 7, old);
                if alias {
                    let new = b.iconst(Type::I8, 211);
                    b.store(p[1], 7, new);
                } else {
                    b.store(p[0], 0, p[2]); // I64 overlaps byte 7
                }
                load(b, p[0], 7)
            });
            assert_eq!(reads(&p, f), 1);
            assert_eq!(run(&p, f, 0xd300_0000_0000_0000), 211);
        }
    }

    #[test]
    fn forwarding_does_not_cross_a_block_or_the_work_bound() {
        for next_block in [false, true] {
            let (p, f) = build(|b, p| {
                let old = b.iconst(Type::I8, 203);
                b.store(p[0], 0, old);
                if next_block {
                    let next = b.create_block(&[]);
                    b.br(next, &[]);
                    b.switch_to(next);
                } else {
                    let mut v = p[2];
                    for _ in 0..LOOKBACK {
                        v = b.int_bin(IntBinOp::Add, v, p[2]);
                    }
                }
                load(b, p[0], 0)
            });
            assert_eq!(reads(&p, f), 1);
            assert_eq!(run(&p, f, 1), 203);
        }
    }

    #[test]
    fn calls_may_change_the_record_through_an_alias() {
        let mut pb = ProgramBuilder::new();
        let writer = pb.declare_func("write", &[Type::Ptr], &[]);
        let mut b = pb.define(writer);
        let byte = b.iconst(Type::I8, 249);
        b.store(b.params()[0], 0, byte);
        b.ret(&[]);
        let f = pb.declare_func("read", &[Type::Ptr, Type::Ptr, Type::I64], &[Type::I64]);
        let mut b = pb.define(f);
        let base = b.params()[0];
        let zero = b.iconst(Type::I8, 0);
        b.store(base, 0, zero);
        b.call(writer, &[base], None);
        let v = load(&mut b, base, 0);
        b.ret(&[v]);
        let p = pb.finish();
        gravity_ir::verify::verify(&p).unwrap();
        assert_eq!(reads(&p, f), 1);
        assert_eq!(run(&p, f, 0), 249);
    }
}
