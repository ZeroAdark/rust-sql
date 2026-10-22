//! The varlena key-image face: the hand-off through which rep.varlena.hash / .order ask the roads key.surrogate, key.tagged
//! and key.coll for a key's image, and L0-d's hand-offs (keyed/coderep.rs, rep/code/order.rs, rep/word/order.rs) reach
//! key.coll (L2; a face file, no rows of its own; M5's face/srf.rs pattern). Decision 4, declared by
//! registry/m6.d/face_varkey.path (cards/M6.md, M6-changes.md §1). Owner M6-TEXT.
//!
//! A road asked while it is on (built, not dropped, not switched off) answers its image or `None` (it does not engage:
//! the caller takes the next form); after the three roads the image is the payload's exact Fp128 (M6 Decision 3a).
// TailForm and `coll` are read by L0-d's hand-offs and rep.hosted.order (Wave 2)
#![allow(dead_code)]
use gravity_decompose::catalog::typefacts::{KeyColl, TypeFacts};
use gravity_ir::{Type, Value};

pub use crate::table::keyimg::{TailForm, VarKeyImpl, VarKeyMech};
use crate::table::keyimg::ImageForm;
use crate::admit::refusal::Site;
use crate::rep::{CellRef, KeyWord, Lx, Scope, Words};

/// The roads in their fixed order (surrogate, canon, tagged, coll), each asked while on; `fp` (the payload's exact Fp128,
/// rep/varlena/hash.rs) last.
pub fn hash_image(lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, coll: KeyColl, scope: Scope, fp: fn(&mut Lx<'_, '_, '_>, CellRef<'_>, &TypeFacts) -> Words) -> Words {
    for form in [ImageForm::Surrogate, ImageForm::Canon, ImageForm::Tagged, ImageForm::Coll] {
        if form.on() {
            if let Some(w) = form.need::<VarKeyImpl>(&*lx).0.hash(lx, c, tf, coll, scope) {
                return w;
            }
        }
    }
    fp(lx, c, tf)
}

/// The order prefix word: the roads in the same order, each asked while on. When none answers, `bytes` (the payload's
/// first 8 bytes, rep/varlena/order.rs) only where they are a prefix of the key's order (a collation that orders by bytes)
/// or its equality word (`KeyWord::Equal` under a collation whose equal values are its equal bytes), as key.coll declines
/// exactly there; under any other collation 0, so every compare is the tail's comparator (the run's collation,
/// `varstr_cmp`). That is key.coll's plain form, and its form while the road is off: the bytes prefix there ordered two
/// keys by their bytes wherever their first 8 bytes differed, the collated tail deciding only ties (`gravity_road_off =
/// 'key.coll'`: 'b' before 'C' under en-x-icu). The tail form rides the run's var table (V4R-267).
pub fn order_prefix(lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, coll: KeyColl, bytes: fn(&mut Lx<'_, '_, '_>, CellRef<'_>, &TypeFacts) -> Value) -> Value {
    for form in [ImageForm::Surrogate, ImageForm::Canon, ImageForm::Tagged, ImageForm::Coll] {
        if form.on() {
            if let Some((w, _)) = form.need::<VarKeyImpl>(&*lx).0.order(lx, c, tf, coll) {
                return w;
            }
        }
    }
    if coll.bytes_order || (lx.key_word == KeyWord::Equal && coll.bytes_eq) {
        return bytes(lx, c, tf);
    }
    lx.b.iconst(Type::I64, 0)
}

/// Whether a key under collation `coll` carries key.coll's ICU prefix word (its sort key's first 8 bytes, `m6_coll_prefix`):
/// the road on and the provider giving a prefix. The forms that order or cut by that word (sort.keylane's radix over a
/// text key, code.cut's collated form) ask this, so with the road off they take their plain form, the word being 0.
pub fn coll_prefix(coll: u32) -> bool {
    ImageForm::Coll.on() && gravity_fn::typesem::collation::prefix_enabled(coll)
}

/// Road key.coll's mechanism (L0-d's hand-offs reach it by name: `NotBuilt` until it is built).
pub fn coll(cx: &impl Site) -> &'static dyn VarKeyMech {
    ImageForm::Coll.need::<VarKeyImpl>(cx).0
}
