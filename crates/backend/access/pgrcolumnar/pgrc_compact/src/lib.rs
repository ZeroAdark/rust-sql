//! A job rewrites its inputs into one part, dropping the rows their DVs mark and carrying every other row's ID forward; the output homes every input's ID ranges.

pub mod ident;
pub mod order;
pub mod policy;
pub mod runs;
pub mod shadow;

use pgrc_format::manifest::{PartRecord, PART_FLAG_COMPACTED};

pub use policy::{next_job, Job, JobKind, PartView, Policy, Reason};

#[derive(Clone, Copy)]
pub enum RowMapView<'a> {
    /// A bulk part: ordinal i has ID `base + i`.
    Implicit(u64),
    Granules(&'a pgrc_format::rowmap::GranMap),
}

impl RowMapView<'_> {
    /// Ordinal `ordinal`'s ID (the caller bounds `ordinal` by the part's rows).
    pub fn ident(&self, ordinal: u32) -> u64 {
        match self {
            RowMapView::Implicit(base) => base + u64::from(ordinal),
            RowMapView::Granules(m) => m.ident(ordinal),
        }
    }

    /// The IDs of ordinals `ordinal..ordinal + out.len()` (one pass; the caller bounds them by the part's rows).
    pub fn decode(&self, ordinal: u32, out: &mut [u64]) {
        match self {
            RowMapView::Implicit(base) => out.iter_mut().enumerate().for_each(|(i, x)| *x = base + u64::from(ordinal) + i as u64),
            RowMapView::Granules(m) => m.decode_ids(ordinal, out),
        }
    }
}

impl PartView {
    /// `bytes` is the file length (the caller may put a decoded-size estimate in its place); `age_s` and `sorted` are the caller's.
    pub fn from_record(rec: &PartRecord, deleted: u64) -> PartView {
        PartView {
            part_no: rec.part_no,
            rows: rec.rows,
            live: rec.rows.saturating_sub(deleted),
            bytes: rec.file_len,
            compacted: rec.flags & PART_FLAG_COMPACTED != 0,
            sorted: false,
            age_s: 0,
        }
    }
}
