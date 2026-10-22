#![allow(unsafe_code)]
use core::ops::{Deref, DerefMut};

pub use gravity_program::arena::{Arena, ArenaBox, ArenaRef, ArenaVec, Mark};

use crate::source::{Locker, SiteCalls, Source, Writer};

/// A source placed in the Node arena with the call targets of its concrete kind (`bind.rs`
/// writes them into the site).
pub struct SourceBox {
    pub(crate) src: ArenaBox<dyn Source>,
    pub(crate) calls: SiteCalls,
}

impl Deref for SourceBox {
    type Target = dyn Source;
    fn deref(&self) -> &(dyn Source + 'static) {
        &*self.src
    }
}

impl DerefMut for SourceBox {
    fn deref_mut(&mut self) -> &mut (dyn Source + 'static) {
        &mut *self.src
    }
}

impl SourceBox {
    /// The concrete source's address: `Site.src`.
    pub(crate) fn data_ptr(&mut self) -> *mut u8 {
        &mut *self.src as *mut dyn Source as *mut u8
    }
}

pub fn box_source<S: Source + 'static>(s: S, a: ArenaRef) -> SourceBox {
    let raw = ArenaBox::into_raw(ArenaBox::new_in(s, a)) as *mut dyn Source;
    SourceBox { src: unsafe { ArenaBox::from_raw_in(raw, a) }, calls: SiteCalls::of::<S>() }
}

pub fn box_writer<W: Writer + 'static>(w: W, a: ArenaRef) -> ArenaBox<dyn Writer> {
    let raw = ArenaBox::into_raw(ArenaBox::new_in(w, a)) as *mut dyn Writer;
    unsafe { ArenaBox::from_raw_in(raw, a) }
}

pub fn box_locker<L: Locker + 'static>(l: L, a: ArenaRef) -> ArenaBox<dyn Locker> {
    let raw = ArenaBox::into_raw(ArenaBox::new_in(l, a)) as *mut dyn Locker;
    unsafe { ArenaBox::from_raw_in(raw, a) }
}
