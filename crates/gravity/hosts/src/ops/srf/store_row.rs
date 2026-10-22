//! M5-PL lever (b) (PL2, M5-plan §2.1): a store row of fixed-width by-value columns deforms from its minimal tuple into
//! the reader's window (sub-file of hosts v4/m5.rs, owner M5-HOSTS). The deform itself is srf/raw.rs `Fixed::deform` (R6).

/// A store row whose columns are all fixed-width by-value at fixed offsets: its minimal tuple, when it holds no NULL,
/// deforms straight into the window (PG's attcacheoff walk, no slot deform); any other row takes the slot.
pub(crate) struct Fixed {
    pub(super) cols: Vec<(usize, i16)>,
    pub(super) end: usize,
}

impl Fixed {
    /// D1217: only the prefix up to the last column the reader needs (`need`, bit c = column c; any column past 63 counts
    /// as needed) must be fixed-width by-value: a column after it is never read, whatever its type (F5's text column).
    pub(crate) fn of(d: &types_tuple::TupleDescData<'_>, natts: usize, need: u64) -> Option<Fixed> {
        if natts > d.natts as usize {
            return None; // the slot path raises "a store window wider than the scan's row"
        }
        let upto = if natts > 64 { natts } else { (0..natts).rev().find(|&c| need >> c & 1 != 0).map_or(0, |c| c + 1) };
        let (mut off, mut cols) = (0usize, Vec::with_capacity(upto)); // alloc: Exec
        // bounded("key-columns: one offset per column of the scan's row up to the last one the reader needs, at most MaxTupleAttributeNumber (1664) columns")
        for c in 0..upto {
            let a = (c < d.natts as usize).then(|| d.compact_attr(c)).filter(|a| a.attbyval && a.attlen > 0 && !a.attisdropped)?;
            off = off.next_multiple_of(a.attalignby.max(1) as usize);
            cols.push((off, a.attlen)); // alloc: Exec
            off += a.attlen as usize;
        }
        Some(Fixed { cols, end: off })
    }
}
