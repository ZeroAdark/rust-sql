//! Sinks for the non-wire destinations and for writes (docs/gravity/exec.md).

use std::rc::Rc;

use gravity_driver::{BorrowedBatch, Flow, Result, Sink};
use gravity_wire::WireNode;
use mcx::Mcx;
use tcop_dest::DestReceiver;
use types_slot::{SlotData, TupleSlotKind};
use types_portal::TuplestoreHandle;
use types_tuple::TupleDescData;

mod raw;

pub struct DestSink {
    dest: *mut DestReceiver<'static>,
    /// D3620: the virtual slot the generic path sends through, made at its first row (`send_row`): the tuplestore and SQL
    /// function receivers take values straight and never use one, and a sink lives on the stack of every nested
    /// statement's run (`Serve::step_counted`), so it holds a word, not the slot.
    slot: Option<Box<SlotData<'static>>>,
    desc: Rc<TupleDescData<'static>>,
    mcx: Mcx<'static>,
    pub rows: u64,
    /// How a row reaches the receiver: straight into a tuplestore, or through the slot (`Direct::Slot`).
    direct: Direct,
    /// A straight row's values and nulls (`Direct::Store` / `Direct::SqlFn`; empty for `Direct::Slot`). D3620: boxed
    /// slices, one pair for either receiver, so the sink (on every nested statement's stack path) stays small.
    vals: Box<[datum::Datum]>,
    nulls: Box<[bool]>,
}

#[derive(Clone, Copy)]
enum Direct {
    /// The generic path: the virtual slot and `receive_slot`.
    Slot,
    /// M5-PL lever (a): a started tuplestore receiver that stores rows as-is takes each row's values straight
    /// (tuplestore_putvalues over the slot's own tupdesc: the minimal tuple and accounting puttupleslot would make)
    Store(TuplestoreHandle),
    /// D1217: a SQL function's receiver (functions.c's DestSqlFunction) whose junk map is the identity and that keeps
    /// every row (its tuplestore; D3628: a function keeping only its first row keeps it in the junkfilter's slot,
    /// `row_slot`) takes each row's values straight, as `sqlfunction_receive` stores them (tuplestore_putvalues over its
    /// clean descriptor): no virtual slot, no receive_slot dispatch, no column copy through the map
    SqlFn,
    /// D3865 L3: a SQL function's receiver keeping only its first row (D3628's `row_slot`, the junkfilter's slot) takes it
    /// straight through its junk map, as `sqlfunction_receive` copies it: no virtual slot made and dropped per execution
    SqlFnRow,
}

impl DestSink {
    pub fn new(mcx: Mcx<'static>, desc: Rc<TupleDescData<'static>>, dest: &mut DestReceiver<'_>) -> DestSink {
        let n = desc.natts as usize;
        let direct = match (dest.plain_tuplestore(), &*dest) {
            (Some(h), _) => Direct::Store(h),
            (None, DestReceiver::SqlFunction(st)) if st.row_slot.is_none() && st.clean_map.len() == n && st.clean_map.iter().enumerate().all(|(i, &a)| a as usize == i + 1) => Direct::SqlFn,
            (None, DestReceiver::SqlFunction(st)) if st.row_slot.is_some() && st.clean_map.iter().all(|&a| a >= 0 && a as usize <= n) => Direct::SqlFnRow,
            _ => Direct::Slot,
        };
        let k = if matches!(direct, Direct::Slot | Direct::SqlFnRow) { 0 } else { n };
        let (vals, nulls) = (vec![datum::Datum::null(); k].into_boxed_slice(), vec![false; k].into_boxed_slice()); // alloc: Exec
        DestSink { dest: dest as *mut DestReceiver<'_> as *mut DestReceiver<'static>, slot: None, desc, mcx, rows: 0, direct, vals, nulls }
    }
}

impl DestSink {
    /// One row to the receiver; false when the receiver wants no more.
    pub fn send_row(&mut self, values: &[u64], isnull: &[u8]) -> types_error::PgResult<bool> {
        if self.rows == 0 {
            gravity_driver::ahead_trace::sent(); // construct C2 R3
        }
        if self.vals.len() == values.len() {
            match self.direct {
                Direct::Store(h) => {
                    self.fill(values, isnull);
                    tuplestore::hold::putvalues(h, &self.desc, &self.vals, &self.nulls)?;
                    self.rows += 1;
                    return Ok(true);
                }
                Direct::SqlFn => {
                    self.fill(values, isnull);
                    if let DestReceiver::SqlFunction(st) = DestSink::dest_mut(&mut self.dest) {
                        tuplestore::hold::putvalues(st.tstore, &st.clean_desc, &self.vals, &self.nulls)?;
                        self.rows += 1;
                        return Ok(true);
                    }
                }
                Direct::SqlFnRow | Direct::Slot => {}
            }
        }
        if matches!(self.direct, Direct::SqlFnRow) && values.len() == self.desc.natts as usize {
            DestSink::keep_first(DestSink::dest_mut(&mut self.dest), values, isnull)?;
            self.rows += 1;
            return Ok(true);
        }
        let (mcx, desc) = (self.mcx, &self.desc);
        let slot = self.slot.get_or_insert_with(|| Box::new(exectuples::make_tuple_table_slot(mcx, TupleSlotKind::Virtual, Some(desc.clone())))); // alloc: Exec
        exectuples::exec_clear_tuple(slot, mcx);
        {
            let base = slot.base_mut();
            for (k, &v) in values.iter().enumerate() {
                base.tts_values[k] = datum::Datum::from_u64(v);
                base.tts_isnull[k] = isnull[k] != 0;
            }
        }
        exectuples::exec_store_virtual_tuple(slot);
        let dest = DestSink::dest_mut(&mut self.dest);
        let keep = dest.receive_slot(slot)?;
        self.rows += 1;
        Ok(keep)
    }
}

impl DestSink {
    /// A straight row's values and nulls into `vals` / `nulls`.
    fn fill(&mut self, values: &[u64], isnull: &[u8]) {
        for (k, (&v, &z)) in values.iter().zip(isnull).enumerate() {
            self.vals[k] = datum::Datum::from_u64(v);
            self.nulls[k] = z != 0;
        }
    }
}

impl Sink for DestSink {
    fn push(&mut self, batch: BorrowedBatch<'_>) -> Result<Flow> {
        for (values, isnull) in batch.iter() {
            if !self.send_row(values, isnull).map_err(gravity_hosts::stash)? {
                return Ok(Flow::Stop);
            }
        }
        Ok(Flow::Continue)
    }
}

/// A statement's receiver, chosen per run: the binding resolves `Program.push` for this one type,
/// so the kernel's push is the receiver's own body behind one predictable match.
pub enum Receiver<'a> {
    Null,
    Dest(DestSink),
    Wire(&'a mut WireNode),
    Cell(CellSink),
    /// The entry's own sink over the run (need 14: `armmerge::ArmMerge` over the statement's receiver).
    Outer(&'a mut dyn Sink),
}

/// An InitPlan program's receiver (rung 12): the first row's first column, the row count (EXISTS stops at one;
/// a scalar link errs past one, as `ExecSetParamPlan` does).
#[derive(Default)]
pub struct CellSink {
    pub exists: bool,
    pub rows: u64,
    pub value: u64,
    pub null: u8,
}

impl Sink for CellSink {
    fn push(&mut self, batch: BorrowedBatch<'_>) -> Result<Flow> {
        for (values, isnull) in batch.iter() {
            self.rows += 1;
            if self.rows == 1 {
                self.value = values.first().copied().unwrap_or(0);
                self.null = isnull.first().copied().unwrap_or(1);
            }
            if self.exists || self.rows > 1 {
                return Ok(Flow::Stop);
            }
        }
        Ok(Flow::Continue)
    }
}

impl Sink for Receiver<'_> {
    fn push(&mut self, batch: BorrowedBatch<'_>) -> Result<Flow> {
        match self {
            Receiver::Null => Ok(Flow::Continue),
            Receiver::Dest(d) => d.push(batch),
            Receiver::Wire(w) => {
                // the block framed whole when every column is an integer; PostgreSQL's own error otherwise (an output function's, e.g. oidvectorout), stashed for `glue::pg_error`
                w.send_rows(batch.values, batch.isnull, batch.rows).map_err(gravity_hosts::stash)?;
                Ok(Flow::Continue)
            }
            Receiver::Cell(c) => c.push(batch),
            Receiver::Outer(o) => o.push(batch),
        }
    }

    fn wire_format(&self) -> Option<gravity_driver::WireFormat> {
        match self {
            Receiver::Wire(w) => w.wire_format(),
            _ => None,
        }
    }

    fn push_wire(&mut self, bytes: &[u8], rows: u64) -> Result<Flow> {
        match self {
            Receiver::Wire(w) => w.send_framed(bytes, rows).map(|_| Flow::Continue).map_err(gravity_hosts::stash),
            Receiver::Null => Ok(Flow::Continue),
            Receiver::Dest(_) => Err(gravity_driver::Error::host("gravity: framed rows to a dest receiver")),
            Receiver::Cell(_) => Err(gravity_driver::Error::host("gravity: framed rows to an InitPlan's cell")),
            Receiver::Outer(_) => Err(gravity_driver::Error::host("gravity: framed rows to the entry's own sink")),
        }
    }
}
