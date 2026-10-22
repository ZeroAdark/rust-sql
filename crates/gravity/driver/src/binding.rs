use core::any::Any;
use core::ops::Deref;
use core::sync::atomic::AtomicU8;

use gravity_abi::{Datum, MorselRange};

use gravity_abi::program::PushFn;

use crate::sink::{LegRef, Sink, SinkRef};

mod raw;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Identity {
    pub proc_number: i32,
    pub resource_owner: u64,
    pub latch: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommandId(pub u32);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PerTuple(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pins(pub u64);

#[derive(Clone, Copy)]
pub struct BindingParts<'a> {
    pub snapshot: Snapshot,
    pub command_id: CommandId,
    pub params: &'a [Datum],
    pub param_nulls: &'a [u8],
    pub abort_flag: &'a AtomicU8,
    pub pins: Pins,
    pub per_tuple: PerTuple,
    pub identity: Identity,
    /// The statement's resolved relations (the hosts' `Resolved`: the part lists and the per-part
    /// faces built once on the leader), read by every source's `bind`; `None` for a statement
    /// without a granule relation.
    pub resolved: Option<&'a (dyn Any + Send + Sync)>,
}

impl BindingParts<'_> {
    pub fn abi_identity(&self) -> gravity_abi::Identity {
        gravity_abi::Identity { proc_number: self.identity.proc_number, command_id: self.command_id.0, resource_owner: self.identity.resource_owner, latch: self.identity.latch, snapshot: self.snapshot.0 }
    }

    pub fn as_ref(&self) -> BindingRef<'_> {
        BindingRef { snapshot: self.snapshot, command_id: self.command_id, abort_flag: self.abort_flag, pins: self.pins, per_tuple: self.per_tuple, identity: self.identity, resolved: self.resolved, params: self.params, ordinal: None }
    }
}

pub struct Binding<'a> {
    pub parts: BindingParts<'a>,
    pub sink: SinkRef<'a>,
    pub rows_processed: u64,
}

pub struct WorkerBinding<'a> {
    pub parts: BindingParts<'a>,
    pub leg: Option<LegRef<'a>>,
    pub rows_processed: u64,
    pub ordinal: u16,
    pub claim: MorselRange,
}

#[derive(Clone, Copy)]
pub struct BindingRef<'a> {
    pub snapshot: Snapshot,
    pub command_id: CommandId,
    pub abort_flag: &'a AtomicU8,
    pub pins: Pins,
    pub per_tuple: PerTuple,
    pub identity: Identity,
    pub resolved: Option<&'a (dyn Any + Send + Sync)>,
    /// The execution's parameter words (`Ctx.params`): M8-WRITE W2's host reads an InitPlan's Init cell handle here.
    pub params: &'a [Datum],
    /// `Some` for a worker slot's hosts (`Binding::fork`): a per-statement count (a scan's `numscans`) is the leader's bind's alone.
    pub ordinal: Option<u16>,
}

impl<'a> Deref for Binding<'a> {
    type Target = BindingParts<'a>;
    fn deref(&self) -> &BindingParts<'a> {
        &self.parts
    }
}

impl<'a> Deref for WorkerBinding<'a> {
    type Target = BindingParts<'a>;
    fn deref(&self) -> &BindingParts<'a> {
        &self.parts
    }
}

impl<'a> Binding<'a> {
    pub fn new<S: Sink>(parts: BindingParts<'a>, sink: &'a mut S) -> Binding<'a> {
        Binding { parts, sink: SinkRef::of(sink), rows_processed: 0 }
    }

    pub fn as_ref(&self) -> BindingRef<'_> {
        self.parts.as_ref()
    }

    pub fn fork<'b>(&'b self, ordinal: u16, claim: MorselRange) -> WorkerBinding<'b> {
        WorkerBinding { parts: self.parts, leg: self.sink.leg(ordinal), rows_processed: 0, ordinal, claim }
    }
}

impl<'a> WorkerBinding<'a> {
    pub fn as_ref(&self) -> BindingRef<'_> {
        BindingRef { ordinal: Some(self.ordinal), ..self.parts.as_ref() }
    }
}

/// What the Node keeps of a binding between attach and detach: the abort byte, the rows it
/// settles per call, and the push target it writes into the program block at attach (the sites'
/// positions settle on the Node: `Node::positions`).
pub trait Attached {
    fn abort(&self) -> &AtomicU8;
    fn add_rows(&mut self, n: u64);
    fn target(&self) -> (PushFn, *mut u8);
}

impl Attached for Binding<'_> {
    fn abort(&self) -> &AtomicU8 {
        self.parts.abort_flag
    }

    fn add_rows(&mut self, n: u64) {
        self.rows_processed += n;
    }

    fn target(&self) -> (PushFn, *mut u8) {
        self.sink.target()
    }
}

impl Attached for WorkerBinding<'_> {
    fn abort(&self) -> &AtomicU8 {
        self.parts.abort_flag
    }

    fn add_rows(&mut self, n: u64) {
        self.rows_processed += n;
    }

    fn target(&self) -> (PushFn, *mut u8) {
        self.leg.map_or(crate::bind::UNBOUND_SINK, |l| l.target())
    }
}
