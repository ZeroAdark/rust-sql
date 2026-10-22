//! VarlenaPtr datum (`rep.varlena.datum`, plan §3.2; KEEP lowerv3 rep/varlena.rs), also a cstring's (`datum_kind`: a
//! `CstrPtr` reads through this arm; its size is `rep/cstr/datum.rs`'s).
//! - `datum`: the varlena pointer the cell carries (a heap page, a pgrc arena, an fmgr result); a lane cell's `val` is its
//!   element's address, holding that pointer.
//! - `retain`: the image copied whole into the consumer's home (`Lx.space`): `VARSIZE_ANY` bytes (a short header, a 4-byte
//!   header compressed or not, an external pointer as its tag's size: what `datumCopy` copies of it); nothing is read for
//!   a NULL. The copy is one host call (`keep_copy`), paid per kept value, never per row read.
//! - no block form (H01): a mask road never holds over a pointer lane.
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::{Rep, Until};

use crate::cell::{Cell, CellHome, CellLife};
use crate::face::kinds::KindId;
use crate::admit::refusal::Reason;
use crate::ops::Table;
use crate::rep::{self, CellRef, DatumArm, Kept, Lx, RetainHome};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RepVarlenaDatum, &ARM)], ..crate::ops::NONE };

pub(crate) const ARM: DatumArm = DatumArm { datum, retain, block: None };

/// `VARTAG_ONDISK`: an external on-disk TOAST pointer's tag (its payload is `varatt_external`, 16 bytes; others' 8).
/// Same value as `types_tuple::varatt::VARTAG_ONDISK`; kept local because lowerv4 does not depend on types_tuple.
const VARTAG_ONDISK: i64 = 18;

fn datum(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, _tf: &TypeFacts) -> (Value, Value) {
    rep::datum_word(cx, c, 8)
}

fn retain(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, until: Until, home: RetainHome) -> Kept {
    let size = if matches!(c.cell.rep, Rep::CstrPtr) { crate::rep::cstr::datum::size } else { varsize_any };
    keep_with(cx, c, tf, until, home, &size, &host_copy)
}

/// The builder, a value's datum pointer: its byte count.
pub(crate) type SizeFn<'s> = &'s dyn Fn(&mut FunctionBuilder<'_>, Value) -> Value;
/// The builder, destination, source, byte count: the copy.
pub(crate) type CopyFn<'s> = &'s dyn Fn(&mut FunctionBuilder<'_>, Value, Value, Value);

/// A by-reference cell kept by copy: `size(b, p)` its byte count (entered only for a non-NULL), `alloc`ed 8-aligned in the
/// consumer's home, copied by `copy`, and the new pointer is the kept cell's datum word.
pub(crate) fn keep_with(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, until: Until, home: RetainHome, size: SizeFn<'_>, copy: CopyFn<'_>) -> Kept {
    if cx.space.is_none() {
        crate::refuse!(cx, Reason::Bug { payload: "a copying retain with no KeepSpace in the context" });
    }
    let (p, null) = rep::datum(cx, c, tf);
    let Lx { b, space, .. } = cx;
    let zero = b.iconst(Type::I64, 0);
    let l_size = b.create_block(&[]);
    let l_sized = b.create_block(&[Type::I64]);
    b.cond_br(null, (l_sized, &[zero]), (l_size, &[]));
    b.switch_to(l_size);
    let n = size(b, p);
    b.br(l_sized, &[n]);
    b.switch_to(l_sized);
    let n = b.block_params(l_sized)[0];
    let seven = b.iconst(Type::I64, 7);
    let up = b.int_bin(IntBinOp::Add, n, seven);
    let mask = b.iconst(Type::I64, !7);
    let bytes = b.int_bin(IntBinOp::And, up, mask);
    let dst = space.as_deref_mut().expect("checked above").alloc(b, home, bytes);
    let dst = if b.value_type(dst) == Type::I64 { dst } else { b.conv(ConvOp::PtrToInt, Type::I64, dst) };
    let l_copy = b.create_block(&[]);
    let l_done = b.create_block(&[]);
    b.cond_br(null, (l_done, &[]), (l_copy, &[]));
    b.switch_to(l_copy);
    copy(b, dst, p, n);
    b.br(l_done, &[]);
    b.switch_to(l_done);
    let val = b.select(null, zero, dst);
    let cell = Cell { life: kept_life(until, home), at: crate::rep::word::datum::off_lane(c.cell.at), ..*c.cell };
    Kept { cell, val, null: Some(null) }
}

/// A kept copy's life: its record's (run's, spool's) life, or the frame slot's `until`.
pub(crate) const fn kept_life(until: Until, home: RetainHome) -> CellLife {
    match home {
        RetainHome::Record(l) | RetainHome::Run(l) | RetainHome::Spool(l) => CellLife { until: Until::Life, home: CellHome::Record(l) },
        RetainHome::Frame => CellLife { until, home: CellHome::FrameSlot },
    }
}

/// The copy as one host call (a length known only per row).
fn host_copy(b: &mut FunctionBuilder<'_>, dst: Value, src: Value, n: Value) {
    let h = b.declare_host_once("rep_keep_copy", &[Type::I64, Type::I64, Type::I64], None, keep_copy as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
    b.call_host(h, &[dst, src, n], None);
}

use super::raw::datum::keep_copy;

/// `VARSIZE_ANY(p)` in IR, reading only the header bytes that exist: a 1-byte header's `b0 >> 1`; an external pointer's
/// `VARHDRSZ_EXTERNAL + VARTAG_SIZE(tag)` (18 on disk, 10 otherwise); a 4-byte header's `u32 >> 2`.
pub(crate) fn varsize_any(b: &mut FunctionBuilder<'_>, p: Value) -> Value {
    let p = b.conv(ConvOp::IntToPtr, Type::Ptr, p);
    let (zero, one, two) = (b.iconst(Type::I64, 0), b.iconst(Type::I64, 1), b.iconst(Type::I64, 2));
    let b0 = b.load_zext(Type::I8, p, 0);
    let short = b.int_bin(IntBinOp::And, b0, one);
    let is_short = b.icmp(IntCond::Ne, short, zero);
    let (l_short, l_long, l_ext, l_out) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(is_short, (l_short, &[]), (l_long, &[]));
    b.switch_to(l_long);
    let w = b.load_zext_unaligned(Type::I32, p, 0);
    let n4 = b.int_bin(IntBinOp::Ushr, w, two);
    b.br(l_out, &[n4]);
    b.switch_to(l_short);
    let is_ext = b.icmp(IntCond::Eq, b0, one);
    let n1 = b.int_bin(IntBinOp::Ushr, b0, one);
    b.cond_br(is_ext, (l_ext, &[]), (l_out, &[n1]));
    b.switch_to(l_ext);
    let tag = b.load_zext(Type::I8, p, 1);
    let ondisk_tag = b.iconst(Type::I64, VARTAG_ONDISK);
    let ondisk = b.icmp(IntCond::Eq, tag, ondisk_tag);
    let (big, small) = (b.iconst(Type::I64, 18), b.iconst(Type::I64, 10));
    let ne = b.select(ondisk, big, small);
    b.br(l_out, &[ne]);
    b.switch_to(l_out);
    b.block_params(l_out)[0]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::cell::{At, Nullable};
    use gravity_decompose::catalog::typefacts::row_of;
    use gravity_interp::Interp;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_program::{LifeId, NumTypmod};

    /// A home that hands out the address in its one argument word.
    pub(crate) struct AtArg(pub Value);
    impl crate::rep::KeepSpace for AtArg {
        fn alloc(&mut self, _b: &mut FunctionBuilder<'_>, _home: RetainHome, _bytes: Value) -> Value {
            self.0
        }
    }

    /// `retain` of a `rep` cell over `src` into `dst`: the kept datum word.
    pub(crate) fn kept(rep: Rep, typid: u32, src: &[u8], dst: &mut [u8]) -> u64 {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let tf = *row_of(typid).or_else(|| row_of(25)).expect("type facts");
        let c = Cell { rep, typid, num: NumTypmod::NONE, life: CellLife { until: Until::Row, home: CellHome::RowCx }, at: At::Value(0), null: Nullable::Maybe };
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("k", &[Type::I64, Type::I64], &[Type::I64]);
        let mut b = pb.define(f);
        let (p, d) = (b.params()[0], b.params()[1]);
        let mut space = AtArg(d);
        let v = {
            let mut cx = Lx::new(&mut b, 1, rig.mcx());
            cx.space = Some(&mut space);
            let k = crate::rep::retain(&mut cx, CellRef { cell: &c, val: p, null: None }, &tf, Until::Life, RetainHome::Record(LifeId(0)));
            assert_eq!(k.cell.life, CellLife { until: Until::Life, home: CellHome::Record(LifeId(0)) });
            k.val
        };
        b.ret(&[v]);
        let prog = pb.finish();
        Interp::new(&prog).run(f, &[src.as_ptr() as u64, dst.as_mut_ptr() as u64]).expect("runs")[0]
    }

    /// `retain` of a varlena cell of `life` over `src` into `home`: the kept word (run by the interpreter, `src` then
    /// scribbled before `dst` is read) and the kept cell's life.
    fn kept_into(life: CellLife, home: RetainHome, src: &mut [u8], dst: &mut [u8]) -> (u64, CellLife) {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let tf = *row_of(25).expect("text's type facts");
        let c = Cell { rep: Rep::VarlenaPtr, typid: 25, num: NumTypmod::NONE, life, at: At::Slot(1), null: Nullable::Maybe };
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("k", &[Type::I64, Type::I64], &[Type::I64]);
        let mut b = pb.define(f);
        let (p, d) = (b.params()[0], b.params()[1]);
        let mut space = AtArg(d);
        let (v, kept) = {
            let mut cx = Lx::new(&mut b, 1, rig.mcx());
            cx.space = Some(&mut space);
            let k = crate::rep::retain(&mut cx, CellRef { cell: &c, val: p, null: None }, &tf, Until::Life, home);
            (k.val, k.cell.life)
        };
        b.ret(&[v]);
        let prog = pb.finish();
        let w = Interp::new(&prog).run(f, &[src.as_ptr() as u64, dst.as_mut_ptr() as u64]).expect("runs")[0];
        src.fill(0xee); // record A freed / reset before B is read
        (w, kept)
    }

    /// M3 L0-a (D323 / D333): a cell of record A retained into record B is copied (A may be reset first), and B's value is
    /// exact; the same cell retained into A itself is not copied.
    #[test]
    fn a_record_cell_reaches_only_its_own_record() {
        let a = CellLife { until: Until::Life, home: CellHome::Record(LifeId(1)) };
        let img = [(4u8 << 1) | 1, b'x', b'y', b'z'];
        let (mut src, mut dst) = (img, [0u8; 16]);
        let (w, life) = kept_into(a, RetainHome::Record(LifeId(2)), &mut src, &mut dst);
        assert_eq!(w, dst.as_ptr() as u64, "record A's bytes are copied into record B");
        assert_eq!(life, CellLife { until: Until::Life, home: CellHome::Record(LifeId(2)) });
        assert_eq!(&dst[..4], &img[..], "B's value is exact after A is reset");
        let (mut src, mut dst) = (img, [0u8; 16]);
        let at = src.as_ptr() as u64;
        let (w, life) = kept_into(a, RetainHome::Record(LifeId(1)), &mut src, &mut dst);
        assert_eq!((w, life), (at, a), "retained into its own record: no copy");
        assert!(dst.iter().all(|&x| x == 0));
        assert!(a.reaches(Until::Life) && !a.reaches(Until::Statement), "a life-less target decides by until as before");
    }

    #[test]
    fn every_header_form_is_kept_whole() {
        let long = [&(((8u32 + 4) << 2).to_le_bytes())[..], b"abcdefgh"].concat();
        for img in [vec![(4u8 << 1) | 1, b'x', b'y', b'z'], long, vec![1, 18, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]] {
            let mut dst = [0u8; 32];
            let v = kept(Rep::VarlenaPtr, 25, &img, &mut dst);
            assert_eq!(v, dst.as_ptr() as u64);
            assert_eq!(&dst[..img.len()], &img[..]);
            assert!(dst[img.len()..].iter().all(|&x| x == 0), "no byte past the image is written");
        }
    }
}
