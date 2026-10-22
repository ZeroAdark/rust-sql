//! A compiled expression's constant (V4R-18's IR accessor, main 09-24). Its value is private to this module: a read is
//! `read` (the bytes or the word decide the program: a by-reference value is logged as baked, `crate::bake`) or `need`
//! (the word plan's: `emit::plan` hands it to the lowerer's word plan, which binds a word per execution from the live
//! plan node or, folding it, logs it there), and no other read compiles.
use core::marker::PhantomData;

use gravity_fn::{Datum, NullableDatum};
use types_core::Oid;

/// D858: a by-value 4-byte word in the convention its type's open-coded forms read: int4 sign-extended (`Int32GetDatum`),
/// oid / xid / cid / reg* zero-extended (`ObjectIdGetDatum`). A plan literal need not be: the planner folds a
/// binary-coercible relabel over a Const into a Const of the result type with the argument's word unchanged
/// (`4294967295::oid::int4`, a custom plan's `$1::int4` over an oid parameter), which C reads through DatumGetInt32 and
/// the open-coded int4 forms read whole. Every other type's word is as given.
pub fn word_canon(typ: Oid, v: Datum) -> Datum {
    match typ {
        23 => Datum::from_i32(v.as_u32() as i32),
        t if gravity_fn::typesem::expr::zext4(t) => Datum::from_u32(v.as_u32()),
        _ => v,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ConstNode<'mcx> {
    value: NullableDatum,
    pub typ: Oid,
    pub typlen: i16,
    pub byval: bool,
    _arena: PhantomData<&'mcx ()>,
}

impl<'mcx> ConstNode<'mcx> {
    pub fn new(value: NullableDatum, typ: Oid, typlen: i16, byval: bool) -> ConstNode<'mcx> {
        let value = if byval && !value.isnull { NullableDatum::value(word_canon(typ, value.value)) } else { value };
        ConstNode { value, typ, typlen, byval, _arena: PhantomData }
    }

    pub fn is_null(&self) -> bool {
        self.value.isnull
    }

    /// The value, read to decide the program: a non-null by-reference one is baked (its image logged).
    pub fn read(&self) -> NullableDatum {
        if !self.byval && !self.value.isnull {
            crate::bake::note(self.value.value.as_usize());
        }
        self.value
    }

    /// The value handed to the word plan (`emit::plan`'s `ParamNeed::Const`): not a bake here. The lowerer binds its
    /// word per execution from the plan node the key walk names; a by-reference need it folds instead, it logs.
    pub fn need(&self) -> NullableDatum {
        self.value
    }

    /// An `= ANY` / `IN` list's array, read for its element count and its elements' needs (`emit::plan`'s
    /// `ParamNeed::Elem`, each a word bound per execution from the plan node): not a bake. Its count decides the shape, and
    /// the keys key it (V4R-110): the front key and the content key hash `list_class` where `list_len` reads the count, the
    /// image where it cannot, so no list's values reach a program's key or its code.
    pub fn list(&self) -> NullableDatum {
        self.value
    }

    /// NEW-72 entry C: whether this non-null varlena constant is the empty string (`text_is_empty`), `None` for a NULL or
    /// another kind. Not a bake: both keys hold it (exec front.rs for every varlena literal; lower key.rs keys `''` by its
    /// image, a bound text comparand by its class: `bound_text_refs`), so a statement whose literal is `''` never shares
    /// the program of one whose literal is not.
    pub fn text_empty(&self) -> Option<bool> {
        // SAFETY: a non-null varlena constant points at a live image.
        (!self.byval && !self.value.isnull && self.typlen == -1).then(|| unsafe { text_is_empty(self.value.value.as_usize()) })
    }

    /// `list_len` of this constant's image (`None`: NULL, by value, or an image the keys take by its bytes).
    pub fn list_len(&self) -> Option<usize> {
        // SAFETY: a non-null by-reference constant points at a live image of its type.
        (!self.byval && !self.value.isnull).then(|| unsafe { list_len(self.value.value.as_usize()) }).flatten()
    }

    /// The list's element count (`list_len`, else its walk over `elem`: an image the keys take by its bytes), not a bake.
    pub fn list_count(&self, elem: crate::nodes::array::ElemFacts) -> Option<usize> {
        let d = self.list();
        (!d.isnull).then(|| self.list_len().or_else(|| crate::nodes::array::Walk::over(d.value, elem, &mut gravity_fn::Region::over(&mut [])).ok().map(|w| w.len()))).flatten()
    }

    /// Every field, the value read (the key stream's walk, D341: a field added here breaks its build).
    pub fn fields(&self) -> (Oid, i16, bool, NullableDatum) {
        let ConstNode { value: _, typ, typlen, byval, _arena: _ } = *self;
        (typ, typlen, byval, self.read())
    }

    /// An island's own copy of the image it read (`Builder::island`): the program keeps these bytes.
    pub(crate) fn own(&mut self, d: Datum) {
        self.value = NullableDatum::value(d);
    }
}

/// The element count of the array image at `image`: its dimensions' product, read from a plain (4-byte header) or a short
/// (1-byte header) image; `None` for a compressed or external one. The count's one reader for the front key, the content
/// key and the lowerer (V4R-110): a list is keyed by `list_class` of it, never by its values.
/// # Safety
/// `image` is a non-null by-reference array constant's value.
pub unsafe fn list_len(image: usize) -> Option<usize> {
    let p = image as *const u8;
    let b0 = *p;
    let hdr = match b0 {
        _ if b0 & 0x03 == 0 => p.add(4),
        0x01 => return None,
        _ if b0 & 0x01 == 1 => p.add(1),
        _ => return None,
    };
    let ndim = hdr.cast::<i32>().read_unaligned();
    (0..ndim.max(0) as usize).try_fold(if ndim <= 0 { 0 } else { 1usize }, |n, k| n.checked_mul(hdr.add(12 + 4 * k).cast::<i32>().read_unaligned().max(0) as usize))
}

/// NEW-72 entry C: whether the varlena image at `image` holds no bytes: a plain or short header of its own length alone. A
/// compressed or external image is never empty. The one reader of the emptiness both keys hold that the per-code road's
/// `''` forms read (percode.rs `classify`, `code_set`).
/// # Safety
/// `image` is a non-null by-reference varlena constant's value.
pub unsafe fn text_is_empty(image: usize) -> bool {
    use types_tuple::varatt;
    let p = image as *const u8;
    if varatt::varatt_is_4b_u(p) {
        varatt::varsize_4b(p) == 4
    } else {
        varatt::varatt_is_1b(p) && !varatt::varatt_is_1b_e(p) && varatt::varsize_1b(p) == 1
    }
}

/// A list's class, what the keys hash for it (V4R-110): its count up to `MAX_LIST` (gravity_expr unrolls that many
/// compares, a word each), one class past it (expr.saop walks or hashes the array's word: any length is one program).
pub fn list_class(n: usize) -> u8 {
    n.min(gravity_program::cost::expr::MAX_LIST + 1) as u8
}

#[cfg(test)]
mod tests {
    /// The count of a plain and a short image of `{7,8,9}` (int4), of an empty array and of a two-dimensional one.
    #[test]
    fn list_len_reads_the_dimensions() {
        let plain: [i32; 9] = [(36 << 2) as i32, 1, 0, 23, 3, 1, 7, 8, 9];
        let img: Vec<u8> = plain.iter().flat_map(|x| x.to_le_bytes()).collect(); // alloc: Cold
        assert_eq!(unsafe { super::list_len(img.as_ptr() as usize) }, Some(3));
        let mut short = vec![((33u8) << 1) | 1]; // alloc: Cold
        short.extend_from_slice(&img[4..]);
        assert_eq!(unsafe { super::list_len(short.as_ptr() as usize) }, Some(3));
        let empty: Vec<u8> = [(16 << 2) as i32, 0, 0, 23].iter().flat_map(|x: &i32| x.to_le_bytes()).collect(); // alloc: Cold
        assert_eq!(unsafe { super::list_len(empty.as_ptr() as usize) }, Some(0));
        let two: Vec<u8> = [(40 << 2) as i32, 2, 0, 23, 2, 3, 1, 1].iter().flat_map(|x: &i32| x.to_le_bytes()).collect(); // alloc: Cold
        assert_eq!(unsafe { super::list_len(two.as_ptr() as usize) }, Some(6));
        let compressed = [0x02u8, 0, 0, 0];
        assert_eq!(unsafe { super::list_len(compressed.as_ptr() as usize) }, None);
        assert_eq!((super::list_class(3), super::list_class(16), super::list_class(17), super::list_class(100_000)), (3, 16, 17, 17));
    }
}
