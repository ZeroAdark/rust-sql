//! Kind `rep.packed.hash` (`HashArm`, cards/M6.md L3, M6-plan §3.1): the key image of a packed numeric lane
//! (`Rep::PackedNum`, a pgrc `numeric(p, s)` column). The image is rep.varlena.hash's exact Fp128 of the cell's numeric
//! datum (rep.packed.datum: the stored image of a verbatim value, else the packed word rendered at the lane's scale), so a
//! hash join over packed lanes at two scales, or a packed lane and a heap numeric, agrees with no common scale decided
//! (`1.5 = 1.50`). Group keys still key by rep.packed.order's word at the lane's one scale (props/group.rs, copy_words.rs
//! `packed_cell`). A NULL row's words are 0 (the image's flags word marks it).
//! - `eq`: the two words against a stored image (never reached on an exact image; kept for callers holding a copy).
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use gravity_program::Rep;

use crate::admit::refusal::Reason;
use crate::face::kinds::KindId;
use crate::ops::Table;
use crate::rep::varlena::hash::{fp_image, ptr};
use crate::rep::{CellRef, HashArm, Lx, Scope, StoredRef, Words};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RepPackedHash, &ARM)], ..crate::ops::NONE };

pub(crate) const ARM: HashArm = HashArm { image, eq };

fn image(lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, _scope: Scope) -> Words {
    if !matches!(c.cell.rep, Rep::PackedNum { .. }) || !tf.image.is_num_form() {
        crate::refuse!(lx, Reason::Bug { payload: "a packed hash image over a cell that is not a packed numeric" });
    }
    fp_image(lx, c, tf)
}

/// Equality against a stored image: the two Fp128 words.
fn eq(lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, stored: StoredRef, tf: &TypeFacts) -> Value {
    let w = image(lx, c, tf, Scope::Program);
    let (Some(lo), Some(hi)) = (w.w[0], w.w[1]) else { crate::refuse!(lx, Reason::Bug { payload: "a packed key image that is not two words" }) };
    let p = ptr(lx.b, stored.ptr);
    let (slo, shi) = (lx.b.load(Type::I64, p, stored.off), lx.b.load(Type::I64, p, stored.off + 8));
    let (a, b) = (lx.b.int_bin(IntBinOp::Xor, lo, slo), lx.b.int_bin(IntBinOp::Xor, hi, shi));
    let d = lx.b.int_bin(IntBinOp::Or, a, b);
    let zero = lx.b.iconst(Type::I64, 0);
    lx.b.icmp(IntCond::Eq, d, zero)
}
