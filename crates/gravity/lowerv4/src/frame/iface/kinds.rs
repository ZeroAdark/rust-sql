//! A kind's registered implementation, read through the interface home (lint RB): the one generic read of the kind
//! registry for a family. A kind with a named accessor in iface.rs (`expr_face`) keeps it.
use crate::admit::refusal::Site;
use crate::face::kinds::KindId;

/// Kind `k`'s registered implementation as its face's type `T` (a maker, a face, an implementation row): `NotBuilt` when
/// no file provides kind `k`, a Bug when one does but not as a `T`, and the mechanisms-used record, exactly as
/// ops/mod.rs `need_kind`.
pub fn kind_impl<T: 'static>(site: &impl Site, k: KindId) -> &'static T {
    crate::frame::iface::need_kind(site, k)
}

#[cfg(test)]
mod tests {
    use crate::face::{kinds::KindId, step::ExprFace};

    /// `kind_impl` is the registry's own row for the kind: the same `&'static` a named accessor and `need_kind` return.
    #[test]
    fn kind_impl_is_the_registered_row() {
        let face: &ExprFace = super::kind_impl(&0i32, KindId::ExprCore);
        assert!(std::ptr::eq(face, crate::frame::iface::expr_face(&0i32)));
        assert!(std::ptr::eq(face, crate::frame::iface::need_kind::<ExprFace>(&0i32, KindId::ExprCore)));
    }
}
