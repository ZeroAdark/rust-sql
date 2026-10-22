//! The logical records of a pgrcolumnar parent: `ROW_DELETE` / `ROW_UPDATE` once Stage B has won, and
//! the bulk lane's rows as `BULK_ROWS`. Every image is flattened, so no record points into a toast table.

use std::cell::{Cell, RefCell};

use mcx::{Mcx, MemoryContext};
use pgrc_format::wal::{DirLoc, TupleImage};
use types_core::xact::XactEvent;
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_rel::{Relation, REPLICA_IDENTITY_FULL};
use types_slot::{SlotData, TupleSlotKind};
use types_tuple::{HeapTupleData, ItemPointerData, SizeofHeapTupleHeader, TupleDescData};

pub const BATCH_ROWS: u32 = 1000;
pub const BATCH_BYTES: usize = 64 * 1024;

pub fn active(rel: &Relation<'_>) -> bool {
    heapam::relation_is_logically_logged(rel)
}

/// Heap's old row for a table without indexes: the whole row under FULL, else none.
pub fn wants_old(relreplident: u8) -> bool {
    relreplident == REPLICA_IDENTITY_FULL
}

fn parent(rel: &Relation<'_>) -> DirLoc {
    let l = rel.rd_locator.get();
    DirLoc { spc: l.spcOid, db: l.dbOid, relnumber: l.relNumber }
}

fn format_error(e: pgrc_format::FormatError) -> Box<PgError> {
    Box::new(PgError::error(format!("pgrc logical record: {e}")))
}

fn image_of<'t>(t: &'t HeapTupleData<'_>) -> TupleImage<'t> {
    let h = t.t_data();
    // SAFETY: a formed or flattened tuple owns `t_len` readable bytes from its header.
    let all = unsafe { std::slice::from_raw_parts(t.header_ptr(), t.t_len as usize) };
    TupleImage { infomask2: h.t_infomask2, infomask: h.t_infomask, hoff: h.t_hoff, data: &all[SizeofHeapTupleHeader..] }
}

fn with_image<R>(mcx: Mcx<'_>, t: &HeapTupleData<'_>, desc: &TupleDescData<'_>, f: impl FnOnce(TupleImage<'_>) -> PgResult<R>) -> PgResult<R> {
    if t.has_external() {
        let flat = heaptoast_seams::toast_flatten_tuple::call(mcx, t, desc)?;
        return f(image_of(flat.as_tuple()));
    }
    f(image_of(t))
}

/// Only after Stage B won: a lost race's orphan version must decode as nothing.
pub fn deleted<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, tid: &ItemPointerData) -> PgResult<()> {
    row(mcx, rel, tid, None)
}

pub fn updated<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, otid: &ItemPointerData, new: &mut SlotData<'mcx>) -> PgResult<()> {
    row(mcx, rel, otid, Some(new))
}

fn row<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, tid: &ItemPointerData, new: Option<&mut SlotData<'mcx>>) -> PgResult<()> {
    let (desc, at) = (&rel.rd_att, parent(rel));
    with_scratch(|cx| {
        let old = match wants_old(rel.rd_rel.relreplident) {
            true => Some(old_row(cx, rel, tid)?),
            false => None,
        };
        let new = new.map(|s| exectuples::exec_copy_slot_heap_tuple(s, mcx, cx)).transpose()?;
        let log = |old: Option<TupleImage<'_>>| match &new {
            None => pgrc_wal_seams::log_row_delete::call(at, old),
            Some(n) => with_image(cx, n.as_tuple(), desc, |n| pgrc_wal_seams::log_row_update::call(at, old, n)),
        };
        match &old {
            Some(o) => with_image(cx, o.as_tuple(), desc, |o| log(Some(o))),
            None => log(None),
        }
        .map(|_| ())
    })
}

fn old_row<'cx>(cx: Mcx<'cx>, rel: &Relation<'cx>, tid: &ItemPointerData) -> PgResult<heaptuple::HeapTuple<'cx>> {
    let mut slot = exectuples::make_tuple_table_slot(cx, TupleSlotKind::Virtual, Some(rel.rd_att.clone()));
    match crate::fetch::fetch_row_version(cx, rel, tid, &None, &mut slot)? {
        true => exectuples::exec_copy_slot_heap_tuple(&mut slot, cx, cx),
        false => Err(Box::new(PgError::error(format!("pgrcolumnar: relation {} has no row at the position it changed", rel.rd_id)))),
    }
}

fn with_scratch<R>(f: impl FnOnce(Mcx<'_>) -> PgResult<R>) -> PgResult<R> {
    let mut cx = SCRATCH.take().unwrap_or_else(|| MemoryContext::new("pgrc logical row"));
    let out = f(cx.mcx());
    cx.reset();
    SCRATCH.set(Some(cx));
    out
}

#[derive(Debug)]
struct Batch {
    relid: Oid,
    parent: DirLoc,
    subid: u32,
    n: u32,
    buf: Vec<u8>,
}

/// Per parent and subtransaction: a record carries the xid of the subtransaction that inserted its rows.
#[derive(Default)]
struct Batches(Vec<Batch>);

impl Batches {
    fn push(&mut self, relid: Oid, parent: DirLoc, subid: u32, image: TupleImage<'_>) -> PgResult<Option<Batch>> {
        let i = match self.0.iter().position(|b| (b.relid, b.subid) == (relid, subid)) {
            Some(i) => i,
            None => {
                self.0.push(Batch { relid, parent, subid, n: 0, buf: Vec::new() });
                self.0.len() - 1
            }
        };
        let b = &mut self.0[i];
        image.encode_into(&mut b.buf).map_err(format_error)?;
        b.n += 1;
        Ok((b.n >= BATCH_ROWS || b.buf.len() >= BATCH_BYTES).then(|| self.0.remove(i)))
    }

    fn take(&mut self, relid: Oid, subid: u32) -> Vec<Batch> {
        let (out, keep) = std::mem::take(&mut self.0).into_iter().partition(|b| (b.relid, b.subid) == (relid, subid));
        self.0 = keep;
        out
    }

    /// Children have the larger ids.
    fn discard(&mut self, subid: u32) {
        self.0.retain(|b| b.subid < subid);
    }

    fn reparent(&mut self, subid: u32, parent: u32) {
        self.0.iter_mut().filter(|b| b.subid == subid).for_each(|b| b.subid = parent);
    }

    fn pending(&self) -> Option<Oid> {
        self.0.first().map(|b| b.relid)
    }
}

thread_local! {
    static BATCHES: RefCell<Batches> = RefCell::new(Batches::default());
    static SCRATCH: Cell<Option<MemoryContext>> = const { Cell::new(None) };
}

fn log(b: Batch) -> PgResult<()> {
    pgrc_wal_seams::log_bulk_rows::call(b.parent, b.n, &b.buf).map(|_| ())
}

pub fn bulk_row<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, slot: &mut SlotData<'mcx>) -> PgResult<()> {
    let subid = ::xact::GetCurrentSubTransactionId();
    let full = with_scratch(|cx| {
        let t = exectuples::exec_copy_slot_heap_tuple(slot, mcx, cx)?;
        with_image(cx, t.as_tuple(), &rel.rd_att, |image| BATCHES.with(|b| b.borrow_mut().push(rel.rd_id, parent(rel), subid, image)))
    })?;
    full.map_or(Ok(()), log)
}

/// Before the publish, as heap logs a row before its commit.
pub fn flush(rel: &Relation<'_>) -> PgResult<()> {
    let subid = ::xact::GetCurrentSubTransactionId();
    BATCHES.with(|b| b.borrow_mut().take(rel.rd_id, subid)).into_iter().try_for_each(log)
}

pub fn at_xact_event(ev: XactEvent) -> PgResult<()> {
    use XactEvent::*;
    match ev {
        XACT_EVENT_PRE_COMMIT | XACT_EVENT_PARALLEL_PRE_COMMIT | XACT_EVENT_PRE_PREPARE => match BATCHES.with(|b| b.borrow().pending()) {
            Some(relid) => Err(Box::new(
                PgError::error(format!("pgrcolumnar: logical rows for relation {relid} reached commit without table_finish_bulk_insert"))
                    .with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR),
            )),
            None => Ok(()),
        },
        _ => {
            BATCHES.with(|b| b.borrow_mut().0.clear());
            Ok(())
        }
    }
}

pub fn at_subxact_abort(subid: u32) {
    BATCHES.with(|b| b.borrow_mut().discard(subid));
}

pub fn at_subxact_commit(subid: u32, parent: u32) {
    BATCHES.with(|b| b.borrow_mut().reparent(subid, parent));
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgrc_format::wal::BulkRowsRec;

    const P: DirLoc = DirLoc { spc: 1663, db: 5, relnumber: 16384 };

    fn img(data: &[u8]) -> TupleImage<'_> {
        TupleImage { infomask2: 2, infomask: 0x0802, hoff: 24, data }
    }

    fn rows(b: &Batch) -> Vec<Vec<u8>> {
        let rec = BulkRowsRec { parent: b.parent, ntuples: b.n, tuples: &b.buf };
        let (mut off, mut out) = (0, Vec::new());
        for _ in 0..b.n {
            let (t, next) = rec.image_at(off).unwrap();
            out.push(t.data.to_vec());
            off = next;
        }
        assert_eq!(off, b.buf.len());
        out
    }

    #[test]
    fn only_full_identity_logs_the_old_row() {
        for (ident, want) in [(b'f', true), (b'd', false), (b'n', false), (b'i', false)] {
            assert_eq!(wants_old(ident), want, "{}", ident as char);
        }
    }

    #[test]
    fn a_batch_goes_out_at_the_row_cap_the_byte_cap_and_statement_end() {
        let mut b = Batches::default();
        for i in 1..BATCH_ROWS {
            assert!(b.push(10, P, 1, img(&i.to_le_bytes())).unwrap().is_none());
        }
        let full = b.push(10, P, 1, img(b"last")).unwrap().expect("the 1000th row fills the batch");
        assert_eq!((full.n, full.parent, rows(&full)[0].clone(), rows(&full)[999].clone()), (BATCH_ROWS, P, 1u32.to_le_bytes().to_vec(), b"last".to_vec()));
        assert_eq!(b.pending(), None);

        let wide = vec![7u8; 22 * 1024];
        assert!(b.push(10, P, 1, img(&wide)).unwrap().is_none() && b.push(10, P, 1, img(&wide)).unwrap().is_none());
        let full = b.push(10, P, 1, img(&wide)).unwrap().expect("past 64 KiB");
        assert_eq!(full.n, 3);
        let huge = vec![1u8; 200 * 1024];
        assert_eq!(b.push(10, P, 1, img(&huge)).unwrap().map(|b| rows(&b)), Some(vec![huge]), "a row past the cap goes alone");

        b.push(10, P, 1, img(b"a")).unwrap();
        b.push(11, P, 1, img(b"b")).unwrap();
        b.push(10, P, 2, img(b"c")).unwrap();
        let got = b.take(10, 1);
        assert_eq!(got.iter().map(rows).collect::<Vec<_>>(), vec![vec![b"a".to_vec()]], "statement end takes its relation in its subtransaction");
        assert!(b.take(10, 1).is_empty());
        assert_eq!(b.pending(), Some(11));
    }

    #[test]
    fn subtransaction_aborts_drop_their_rows_and_commits_hand_them_up() {
        let mut b = Batches::default();
        b.push(10, P, 1, img(b"top")).unwrap();
        b.push(10, P, 2, img(b"kept")).unwrap();
        b.push(10, P, 3, img(b"aborted")).unwrap();
        b.push(10, P, 4, img(b"aborted child")).unwrap();
        b.discard(3);
        b.reparent(2, 1);
        let got: Vec<Vec<Vec<u8>>> = b.take(10, 1).iter().map(rows).collect();
        assert_eq!(got, vec![vec![b"top".to_vec()], vec![b"kept".to_vec()]]);
        assert_eq!(b.pending(), None);
        b.push(12, P, 1, img(b"x")).unwrap();
        b.discard(1);
        assert_eq!(b.pending(), None, "the top level's abort drops everything");
    }
}
