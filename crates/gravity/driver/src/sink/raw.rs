//! `sink`'s unsafe steps (rule R6, the module's raw home): the bound sink and a leg's share behind an opaque pointer,
//! called through the functions monomorphised for their type, and the shared breaker region's Send / Sync.
#![allow(unsafe_code)]

use core::marker::PhantomData;

use gravity_abi::program::PushFn;

use super::*;

pub struct SinkCalls {
    pub push: PushFn,
    pub batch: unsafe fn(*mut u8, BorrowedBatch<'_>) -> Result<Flow>,
    pub leg: unsafe fn(*mut u8, u16) -> Option<LegRef<'static>>,
    pub ordered: unsafe fn(*mut u8) -> bool,
    pub drain: unsafe fn(*mut u8, &[u64]) -> Result<Flow>,
    pub buffered: unsafe fn(*mut u8) -> u64,
}

struct Calls<S>(PhantomData<S>);

impl<S: Sink> Calls<S> {
    const OF: SinkCalls = SinkCalls { push: Some(crate::hostimpl::sink::push::<S>), batch: Self::batch, leg: Self::leg, ordered: Self::ordered, drain: Self::drain, buffered: Self::buffered };

    unsafe fn buffered(p: *mut u8) -> u64 {
        (*(p as *mut S)).buffered()
    }

    unsafe fn ordered(p: *mut u8) -> bool {
        (*(p as *mut S)).ordered()
    }

    unsafe fn drain(p: *mut u8, los: &[u64]) -> Result<Flow> {
        (*(p as *mut S)).drain_ready(los)
    }

    unsafe fn batch(p: *mut u8, b: BorrowedBatch<'_>) -> Result<Flow> {
        (*(p as *mut S)).push(b)
    }

    unsafe fn leg(p: *mut u8, k: u16) -> Option<LegRef<'static>> {
        (*(p as *mut S)).leg(k).map(|l| LegRef { ptr: l.ptr, push: l.push, claims: l.claims, _p: PhantomData })
    }
}

struct NoSink;

impl Sink for NoSink {
    fn push(&mut self, _batch: BorrowedBatch<'_>) -> Result<Flow> {
        Err(crate::Error::host("gravity: a push from a merge step with no sink (phase 2's layout)"))
    }
}

/// The bound sink: its state behind an opaque pointer, the calls monomorphised for its type; `push` here is the cold driver-side one (a merge's finish, EPQ's row), the kernel's is the block's.
#[derive(Clone, Copy)]
pub struct SinkRef<'a> {
    ptr: *mut u8,
    calls: &'static SinkCalls,
    _p: PhantomData<&'a mut ()>,
}

impl<'a> SinkRef<'a> {
    pub fn of<S: Sink>(sink: &'a mut S) -> SinkRef<'a> {
        SinkRef { ptr: sink as *mut S as *mut u8, calls: &Calls::<S>::OF, _p: PhantomData }
    }

    /// A sink that refuses every push (phase 2's layout, where no row may leave: `Merge::prepare_partitions_in`).
    pub(crate) fn none() -> SinkRef<'static> {
        SinkRef { ptr: core::ptr::NonNull::<NoSink>::dangling().as_ptr() as *mut u8, calls: &Calls::<NoSink>::OF, _p: PhantomData }
    }

    pub fn push(&mut self, batch: BorrowedBatch<'_>) -> Result<Flow> {
        unsafe { (self.calls.batch)(self.ptr, batch) }
    }

    pub fn leg(&self, ordinal: u16) -> Option<LegRef<'a>> {
        unsafe { (self.calls.leg)(self.ptr, ordinal) }.map(|l| LegRef { ptr: l.ptr, push: l.push, claims: l.claims, _p: PhantomData })
    }

    pub fn target(&self) -> (PushFn, *mut u8) {
        (self.calls.push, self.ptr)
    }

    pub fn ordered(&self) -> bool {
        unsafe { (self.calls.ordered)(self.ptr) }
    }

    pub fn drain_ready(&mut self, los: &[u64]) -> Result<Flow> {
        unsafe { (self.calls.drain)(self.ptr, los) }
    }

    pub fn buffered(&self) -> u64 {
        unsafe { (self.calls.buffered)(self.ptr) }
    }
}

#[derive(Clone, Copy)]
pub struct LegRef<'a> {
    ptr: *const u8,
    push: PushFn,
    claims: &'static LegClaims,
    _p: PhantomData<&'a ()>,
}

pub struct LegClaims {
    begin: unsafe fn(*const u8, gravity_abi::MorselRange),
    end: unsafe fn(*const u8, bool),
}

struct ClaimCalls<L>(PhantomData<L>);

impl<L: SinkLeg> ClaimCalls<L> {
    const OF: LegClaims = LegClaims { begin: Self::begin, end: Self::end };

    unsafe fn begin(p: *const u8, c: gravity_abi::MorselRange) {
        (*(p as *const L)).claim_begin(c)
    }

    unsafe fn end(p: *const u8, ok: bool) {
        (*(p as *const L)).claim_end(ok)
    }
}

impl<'a> LegRef<'a> {
    pub fn of<L: SinkLeg>(leg: &'a L) -> LegRef<'a> {
        LegRef { ptr: leg as *const L as *const u8, push: Some(crate::hostimpl::sink::push_leg::<L>), claims: &ClaimCalls::<L>::OF, _p: PhantomData }
    }

    pub fn claim_begin(&self, claim: gravity_abi::MorselRange) {
        unsafe { (self.claims.begin)(self.ptr, claim) }
    }

    pub fn claim_end(&self, ok: bool) {
        unsafe { (self.claims.end)(self.ptr, ok) }
    }

    pub fn target(&self) -> (PushFn, *mut u8) {
        (self.push, self.ptr as *mut u8)
    }
}

unsafe impl Send for Region {}
unsafe impl Sync for Region {}
