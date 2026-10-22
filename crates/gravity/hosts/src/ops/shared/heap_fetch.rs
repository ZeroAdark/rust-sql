use std::rc::Rc;

use datum::Datum;
use gravity_abi::Datum as Word;
use gravity_program::protos::{CMAX_AT, SYS_AT_BASE, XMIN_AT};
use mcx::Mcx;
use types_error::PgResult;
use types_rel::Relation;
use types_relscan::IndexScanDescData;
use types_scan::sdir::ScanDirection;
use types_slot::SlotData;
use types_snapshot::SnapshotData;
use types_tuple::ItemPointerData;

pub const SELF_ITEM_POINTER: i16 = -1;

/// `project`'s reading of a site's `attnos`, made once per site, not per row: the highest table column read, and whether
/// every cell is a table column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Proj {
    last: i16,
    plain: bool,
}

impl Proj {
    pub fn of(attnos: &[i16]) -> Proj {
        let (mut last, mut plain) = (0i16, true);
        // bounded("key-columns: one attno test per column the site reads, at most MaxTupleAttributeNumber (1664) columns")
        for &a in attnos {
            if a > 0 && !is_sys(a) {
                last = last.max(a);
            } else {
                plain = false;
            }
        }
        Proj { last, plain }
    }

    /// The highest table column read (0: none).
    pub fn last(self) -> i16 {
        self.last
    }

    /// Whether every cell is a table column (no `ctid`, no system lane).
    pub fn plain(self) -> bool {
        self.plain
    }
}

/// The fetched row's cells by `attnos`: a table column, `ctid` (`SELF_ITEM_POINTER`: the slot's tid) or a V4R-88 system
/// lane (`XMIN_AT` ..= `CMAX_AT`: C `slot_getsysattr` over the slot's heap tuple, as heap_window.rs fills a page window's).
pub fn project(slot: &mut SlotData<'_>, attnos: &[i16], values: &mut [Word], isnull: &mut [u8]) -> PgResult<()> {
    project_by(slot, attnos, Proj::of(attnos), values, isnull)
}

/// `project` with the site's reading of `attnos` (`Proj::of(attnos)`).
pub fn project_by(slot: &mut SlotData<'_>, attnos: &[i16], p: Proj, values: &mut [Word], isnull: &mut [u8]) -> PgResult<()> {
    debug_assert_eq!(p, Proj::of(attnos));
    // D1217: one deform to the highest attribute read, then each read hits the slot's valid prefix (slot_getattr per
    // attribute ascending re-entered the deform once per column: slot_getattr_miss on every row)
    let Proj { last, plain } = p;
    if last > 0 {
        exectuples::slot_getsomeattrs(slot, last as i32);
    }
    if plain {
        // every cell a table column: read the deformed prefix's arrays once, not the slot per column
        let base = slot.base();
        let (vals, nulls) = (&base.tts_values[..last as usize], &base.tts_isnull[..last as usize]);
        // bounded("key-columns: one cell per output column of a row, at most MaxTupleAttributeNumber (1664) columns")
        for ((&a, v), n) in attnos.iter().zip(values.iter_mut()).zip(isnull.iter_mut()) {
            *v = vals[a as usize - 1].as_u64();
            *n = nulls[a as usize - 1] as u8;
        }
        return Ok(());
    }
    // bounded("key-columns: one cell per output column of a row, at most MaxTupleAttributeNumber (1664) columns")
    for (k, &attno) in attnos.iter().enumerate() {
        let mut null = false;
        let d: Datum = if attno == SELF_ITEM_POINTER {
            Datum::from_usize(&slot.base().tts_tid as *const _ as usize)
        } else if is_sys(attno) {
            exectuples::slot_getsysattr(slot, (SYS_AT_BASE - attno) as i32, &mut null)?
        } else {
            exectuples::slot_getattr(slot, attno as i32, &mut null)
        };
        values[k] = d.as_u64();
        isnull[k] = null as u8;
    }
    Ok(())
}

pub fn fetch_row<'mcx>(mcx: Mcx<'mcx>, scan: &mut IndexScanDescData<'mcx>, slot: &mut SlotData<'mcx>, attnos: &[i16], values: &mut [Word], isnull: &mut [u8]) -> PgResult<bool> {
    if !indexam::index_getnext_slot(mcx, scan, ScanDirection::ForwardScanDirection, slot)? {
        return Ok(false);
    }
    project(slot, attnos, values, isnull)?;
    Ok(true)
}

pub fn fetch_by_tid<'mcx>(mcx: Mcx<'mcx>, heap: &Relation<'mcx>, tid: &ItemPointerData, snapshot: &Rc<SnapshotData<'mcx>>, slot: &mut SlotData<'mcx>, attnos: &[i16], values: &mut [Word], isnull: &mut [u8]) -> PgResult<bool> {
    let snap = Some(snapshot.clone());
    if !tableam::table_tuple_fetch_row_version(mcx, heap, tid, &snap, slot)? {
        return Ok(false);
    }
    project(slot, attnos, values, isnull)?;
    Ok(true)
}

/// The tuple at `tid` regardless of visibility (SnapshotAny): the EPQ test
/// tuple is the newest version the lock host already found committed.
pub fn fetch_any<'mcx>(mcx: Mcx<'mcx>, heap: &Relation<'mcx>, tid: &ItemPointerData, slot: &mut SlotData<'mcx>) -> PgResult<bool> {
    tableam::table_tuple_fetch_row_version(mcx, heap, tid, &None, slot)
}

/// V4R-88: a system lane's attno (`XMIN_AT` ..= `CMAX_AT`, gravity_program::v4), filled from the tuple header.
pub fn is_sys(a: i16) -> bool {
    (XMIN_AT..=CMAX_AT).contains(&a)
}
