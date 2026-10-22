//! A `name` key beside a `text` key in a hash join (keys.rs `check_keys` / `key_src`). PostgreSQL's cross-type `=` of
//! text_ops (nameeqtext / texteqname) compares the name's bytes up to its NUL with the text's payload under a
//! deterministic collation (texteq of the name's text under any other), and text_ops' hash family hashes the two sides
//! alike (hashname / hashtext read the same bytes). So the name side keys as its text: its bytes before the NUL (at most
//! NAMEDATALEN - 1) copied behind a 4-byte varlena header into a stack slot, a text cell the varlena arm images as it
//! images the other side's (the same roads, the same Fp128: rep/varlena/hash.rs). Owner M1-JOIN. Budget 90 lines.
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_program::{NumTypmod, Rep, Until};
use types_core::catalog::{NAMEOID, TEXTOID};

use crate::admit::refusal::{Reason, ShapeWhy};
use crate::cell::{At, Cell, CellHome, CellLife};
use crate::rep::{CellRef, Lx};

/// A name's storage (NAMEDATALEN): the bytes the copy reads, its NUL among them.
const NAME_BYTES: i32 = 64;

/// Key facts `tf` are a name's (the side keysem `image::name_text` pairs with a text, read as its text).
pub(crate) fn is_name(tf: &TypeFacts) -> bool {
    tf.base == NAMEOID
}

/// The text cell of name key cell `c` (a name's 64 bytes, `Rep::FixedPtr`): the bytes before its NUL behind a 4-byte
/// varlena header (SET_VARSIZE: `(len + 4) << 2`) in a stack slot, live for the row. A NULL row copies the zeroed slot
/// (its image words are 0 either way: the image's flags word marks it). A name in any other form (a dictionary code)
/// refuses by name.
pub(crate) fn text_cell(lx: &mut Lx<'_, '_, '_>, node: i32, &(cell, val, null, tf): &(Cell, Value, Option<Value>, TypeFacts)) -> (Cell, Value, Option<Value>, TypeFacts) {
    let Some(text) = typefacts::row_of(TEXTOID).copied() else { crate::refuse!(node, Reason::Bug { payload: "text's type facts" }) };
    if !matches!(cell.rep, Rep::FixedPtr { len } if i32::from(len) == NAME_BYTES) {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("a name join key beside a text key whose name is not its 64 bytes (a dictionary code)") });
    }
    let (p, _) = crate::rep::datum(lx, CellRef { cell: &cell, val, null }, &tf);
    let n0 = lx.b.num_blocks();
    let b = &mut *lx.b;
    let slot = b.declare_stack_slot(8 + NAME_BYTES as u32, 8);
    let base = b.stack_addr(slot);
    let zero = b.iconst(Type::I64, 0);
    for off in (8..8 + NAME_BYTES).step_by(8) {
        b.store(base, off, zero);
    }
    let body = b.ptr_add(base, zero, 1, 8);
    let src = match null {
        Some(n) => {
            let z = b.conv(ConvOp::PtrToInt, Type::I64, body);
            b.select(n, z, p)
        }
        None => p,
    };
    let src = b.conv(ConvOp::IntToPtr, Type::Ptr, src);
    for off in (0..NAME_BYTES).step_by(8) {
        let w = b.load_unaligned(Type::I64, src, off);
        b.store(body, off, w);
    }
    // strnlen: the bytes before the first NUL (a name always holds one; the bound keeps the loop finite)
    let (l_head, l_byte, l_done) = (b.create_block(&[Type::I64]), b.create_block(&[]), b.create_block(&[Type::I64]));
    b.br(l_head, &[zero]);
    b.switch_to(l_head);
    b.mark_loop_header(l_head);
    let i = b.block_params(l_head)[0];
    let lim = b.iconst(Type::I64, i64::from(NAME_BYTES));
    let end = b.icmp(IntCond::Eq, i, lim);
    b.cond_br(end, (l_done, &[i]), (l_byte, &[]));
    b.switch_to(l_byte);
    let at = b.ptr_add(body, i, 1, 0);
    let c = b.load_zext(Type::I8, at, 0);
    let nul = b.icmp(IntCond::Eq, c, zero);
    let one = b.iconst(Type::I64, 1);
    let next = b.int_bin(IntBinOp::Add, i, one);
    b.cond_br(nul, (l_done, &[i]), (l_head, &[next]));
    b.switch_to(l_done);
    let len = b.block_params(l_done)[0];
    let (four, two) = (b.iconst(Type::I64, 4), b.iconst(Type::I64, 2));
    let size = b.int_bin(IntBinOp::Add, len, four);
    let header = b.int_bin(IntBinOp::Shl, size, two);
    let header = b.conv(ConvOp::Trunc, Type::I32, header);
    b.store(base, 4, header);
    let v = b.ptr_add(base, zero, 1, 4);
    let v = b.conv(ConvOp::PtrToInt, Type::I64, v);
    lx.memo.bounded_since(lx.b, n0);
    let life = CellLife { until: Until::Row, home: CellHome::RowCx };
    (Cell { rep: Rep::VarlenaPtr, typid: TEXTOID, num: NumTypmod::NONE, life, at: At::Value(0), null: cell.null }, v, null, text)
}

#[cfg(test)]
mod tests {
    use gravity_decompose::catalog::keysem::image::name_text;
    use gravity_decompose::catalog::NoFacts;
    use gravity_decompose::catalog::typefacts::{KeyColl, C_COLLATION, DEFAULT_COLLATION, FIRST_NORMAL_OID};

    #[test]
    fn a_name_pairs_with_text_or_varchar_under_a_set_deterministic_collation_only() {
        let (name, text, varchar, bpchar, bytea) = (19, 25, 1043, 1042, 17);
        let (c, d) = (KeyColl::of(C_COLLATION), KeyColl::of(DEFAULT_COLLATION));
        assert!(name_text(&NoFacts, (name, text), c) && name_text(&NoFacts, (text, name), c));
        assert!(name_text(&NoFacts, (varchar, name), d));
        for t in [name, bpchar, bytea] {
            assert!(!name_text(&NoFacts, (name, t), c), "{t}");
        }
        assert!(!name_text(&NoFacts, (text, text), c));
        assert!(!name_text(&NoFacts, (name, text), KeyColl::of(0)), "an unset collation raises in PostgreSQL");
        assert!(!name_text(&NoFacts, (name, text), KeyColl::of(FIRST_NORMAL_OID)), "a nondeterministic collation");
    }
}
