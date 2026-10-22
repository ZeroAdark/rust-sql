//! The frame's raw half: the one lifetime step a lowering takes for nodes it builds at compile time, a context the
//! program owns lent at the program's lifetime (step/expr_row.rs `record_expr`, ops/mergejoin/strategies/markable.rs
//! `compares`); the faces' state slots read back as their types (`Exts`).
#![allow(unsafe_code)]
use core::any::TypeId;

use crate::face::kinds::Face;
use crate::frame::px::{Exts, Held, Px};

/// A face's compile-time state (`Px::ext_mut`), made in the lowering's planning context at first use and dropped with the
/// `Px`; the slot finds it by `TypeId::of::<Id>()`.
///
/// # Safety
/// `Id` is `Self` with every lifetime `'static`, and the impl is for `Self<'g>` exactly (a type with a lifetime has the one,
/// `'g`): two types never share an `Id`, so a slot whose id is `T::Id` holds a `T`. `ext_g!` writes such an impl.
pub unsafe trait ExtG<'g>: 'g {
    type Id: 'static;
    fn make(mcx: mcx::Mcx<'g>) -> Self;
}

/// `ExtG` for a state type: `ext_g!(T)` (no lifetime, made by `Default`), `ext_g!(T<'g>, make)` (`make(mcx)`).
macro_rules! ext_g {
    ($t:ident) => {
        // SAFETY: ExtG's contract: `Id` is the type itself, which has no lifetime.
        #[allow(unsafe_code)]
        unsafe impl<'g> $crate::frame::px::ExtG<'g> for $t {
            type Id = $t;
            fn make(_: mcx::Mcx<'g>) -> $t {
                <$t as Default>::default()
            }
        }
    };
    ($t:ident<'g>, $make:expr) => {
        // SAFETY: ExtG's contract: the impl is for `$t<'g>` itself and `Id` is `$t<'static>`.
        #[allow(unsafe_code)]
        unsafe impl<'g> $crate::frame::px::ExtG<'g> for $t<'g> {
            type Id = $t<'static>;
            fn make(m: mcx::Mcx<'g>) -> $t<'g> {
                ($make)(m)
            }
        }
    };
}
pub(crate) use ext_g;

impl<'p> Px<'_, 'p> {
    /// A context `name` kept in `owned` for the program's life, lent at the program's lifetime `'p`: nodes built in it are
    /// read by this statement's compiles and live as long as the program. planprod P4: a child of the program's hosted
    /// product root (`hosted_root`, made here at the first call), so it is the statement's while the lowering builds it and
    /// its holder's once the owner is filed (`hir::adopt`).
    pub(crate) fn program_cx(&mut self, name: &'static str) -> mcx::Mcx<'p> {
        let root = self.hosted_root.get_or_insert_with(crate::hir::hosted_root);
        let cx = Box::new(root.new_child(name)); // alloc: Program
        // SAFETY: the context is boxed, so its address holds when the box moves into `owned`, which the program keeps
        // (frame/program.rs takes it into the hosted owner, whose root, its parent, drops after it) for as long as `'p`.
        let m: mcx::Mcx<'p> = unsafe { core::mem::transmute::<mcx::Mcx<'_>, mcx::Mcx<'p>>(cx.mcx()) };
        self.owned.push(cx); // alloc: Program
        m
    }

    /// memsig E-prog: the lowering's lists root (`lists_root`, made here at the first call, owned by the running
    /// statement), lent at the program's lifetime `'p`: a compiled expression's program builds its lists in it, in place.
    pub(crate) fn lists_mcx(&mut self) -> mcx::Mcx<'p> {
        let root = self.lists_root.get_or_insert_with(|| mcx::SendRoot::new_owned_small("GravityHostedLists", memledger::charge::statement_owner()));
        let ctx = root.mcx().context();
        // SAFETY: a SendRoot holds its context box raw, so the address holds when the root moves (the finish takes it,
        // `hir::finish_keeping`); every block made through the handle is a list of a program in `exs` or one dropped before
        // the root: the `Px` drops `exs` first (the root is its last field), and the finish compacts every list out of it
        // (`Program::into_hosted`) before it drops the root.
        let ctx: &'p mcx::MemoryContext = unsafe { &*(ctx as *const mcx::MemoryContext) };
        ctx.mcx()
    }
}

/// memsig E-prog: `rep::expr_type::result_type`'s compile of `node` (its type when the decomposer's names none): the
/// program's lists in a scratch root of the running statement's ('GravityTypeProbe'), which outlives the program read and
/// dropped here.
pub(crate) fn compiled_type<'p>(gx: &crate::graph::Gx<'_, 'p>, node: types_nodes::Node<'p>, id: i32) -> types_core::Oid {
    let mut root = mcx::SendRoot::new_owned_small("GravityTypeProbe", memledger::charge::statement_owner());
    let ctx = root.mcx().context();
    // SAFETY: a SendRoot holds its context box raw (address-stable); the compiled program (its lists in it) is a temporary
    // of the statement below and drops before `root`
    let ctx: &'p mcx::MemoryContext = unsafe { &*(ctx as *const mcx::MemoryContext) };
    let t = crate::step::expr::compiled(ctx.mcx(), gx, node, None, id, None, None).result_type;
    drop(root);
    t
}

impl<'g> Exts<'g> {
    /// Face `f`'s state if it is a `T`.
    pub(crate) fn get<T: ExtG<'g>>(&self, f: Face) -> Option<&T> {
        let (id, b) = self.slots[f as usize].as_ref()?;
        let p = &**b as *const (dyn Held + 'g) as *const T;
        // SAFETY: ExtG::Id is the 'static twin, one Id per ext type (the unsafe trait's contract), and the value never
        // outlives 'g: the slot holds what `get_or_make` put with its `T::Id`, so an equal id is a `T` (at this `'g`:
        // `Exts` is invariant in it), alive in its 'g box while `self` is borrowed.
        (*id == TypeId::of::<T::Id>()).then(|| unsafe { &*p })
    }

    pub(crate) fn get_mut<T: ExtG<'g>>(&mut self, f: Face) -> Option<&mut T> {
        let (id, b) = self.slots[f as usize].as_mut()?;
        let p = &mut **b as *mut (dyn Held + 'g) as *mut T;
        // SAFETY: as in `get` (ExtG::Id is the 'static twin, one Id per ext type, the value never outlives 'g), the box
        // borrowed mutably through `self`.
        (*id == TypeId::of::<T::Id>()).then(|| unsafe { &mut *p })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use mcx::PgVec;

    use super::*;

    #[derive(Default)]
    struct Count(u32);
    crate::frame::px::ext_g!(Count);

    struct Kept<'g>(PgVec<'g, u64>, Option<Rc<Cell<u32>>>);
    impl Drop for Kept<'_> {
        fn drop(&mut self) {
            if let Some(d) = &self.1 {
                d.set(d.get() + 1);
            }
        }
    }
    crate::frame::px::ext_g!(Kept<'g>, |m| Kept(PgVec::new_in(m), None));

    /// A face's slot read as another state type is `None` (`Px::ext`), and as its own type it is the state set.
    #[test]
    fn a_face_slot_read_as_another_type_is_none() {
        let cx = gravity_driver::mem::root::context("GravityPlan", true);
        let mut e = Exts::default();
        e.get_or_make::<Count>(Face::Join, cx.mcx()).unwrap().0 = 3;
        assert!(e.get::<Kept<'_>>(Face::Join).is_none());
        assert!(e.get_mut::<Kept<'_>>(Face::Join).is_none());
        assert!(e.get_or_make::<Kept<'_>>(Face::Join, cx.mcx()).is_none());
        assert_eq!(e.get::<Count>(Face::Join).map(|c| c.0), Some(3));
        assert!(e.get::<Count>(Face::Nest).is_none());
    }

    /// The state set inside a plan lives in the plan's context and is dropped with the slots, its bytes given back.
    #[test]
    fn a_face_state_is_dropped_with_the_plan_context() {
        let cx = gravity_driver::mem::root::context("GravityPlan", true);
        let drops = Rc::new(Cell::new(0));
        let before = cx.used();
        let mut e = Exts::default();
        let k = e.get_or_make::<Kept<'_>>(Face::End, cx.mcx()).unwrap();
        k.1 = Some(Rc::clone(&drops));
        k.0.extend_from_slice(&[1, 2, 3]);
        assert!(cx.used() > before);
        assert_eq!(e.get::<Kept<'_>>(Face::End).map(|k| k.0.len()), Some(3));
        drop(e);
        assert_eq!(drops.get(), 1);
        assert_eq!(cx.used(), before);
    }
}
