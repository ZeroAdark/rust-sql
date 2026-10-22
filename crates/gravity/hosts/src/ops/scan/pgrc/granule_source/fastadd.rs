//! FASTADD: a pgrcolumnar table after a metadata-only ADD COLUMN holds parts written before
//! it, which lack the column (the manifest lists them under their older schema), and a reader whose snapshot holds a
//! generation from before the ALTER reads it under the relation's new descriptor. The granule source reads stored
//! streams only, so each such (part, column) is read from a stand-in part (`pgrc_am::fastadd::Lacking`): one column,
//! every row the column's `attmissingval` (NULL without one), sealed in memory at the part's geometry. A list whose
//! parts lack a column of the reader is a derived list ([`reader_list`]): the base list's parts and answers, its own
//! faces (a stand-in's Stats, dictionaries and bloom / PSMA bodies live with it), and a generation that mixes in what
//! the stand-ins hold, so nothing keyed by a list's generation (the zone store) or a part's content (the condition
//! cache, fingerprint tables) is shared with a list that reads other fills.

use std::sync::Arc;

use pgrc_read::OpenPart;
use types_error::PgResult;
use types_rel::Relation;

use super::{fnv, PartIoSpec, PartList, PartSpec};

/// `list` as `rel`'s descriptor reads it: `list` itself when every part holds every column of the relation (the
/// common case: one schema, the relation's), else the derived list with the stand-ins, built once per base list and
/// what the stand-ins hold.
pub(super) fn reader_list(rel: &Relation<'_>, list: Arc<PartList>) -> PgResult<Arc<PartList>> {
    if list.parts.is_empty() || list.lacking.is_some() {
        return Ok(list);
    }
    let fps: Vec<Option<u64>> = list.parts.iter().map(|p| p.expect.schema_fingerprint).collect(); // alloc: Exec
    let Some(lacking) = pgrc_am::fastadd::Lacking::of(rel, &fps)? else { return Ok(list) };
    let digest = lacking.digest();
    let mut derived = pgsync::lock(&list.derived);
    if let Some((_, d)) = derived.iter().find(|(k, _)| *k == digest) {
        return Ok(Arc::clone(d));
    }
    let parts = list.parts.iter().map(|p| PartSpec { io: io_of(&p.io), expect: p.expect, rows: p.rows, granules: p.granules, deletes: p.deletes.clone(), part: p.part.clone() }).collect(); // alloc: Cold
    let mut d = PartList::with_answers(parts, list.answers.clone());
    d.generation = fnv(d.generation, &digest.to_le_bytes());
    d.lacking = Some(Arc::new(lacking)); // alloc: Cold
    let d = Arc::new(d); // alloc: Cold
    // a base list serves a few descriptors over its life (one per ADD / DROP COLUMN after it): the oldest goes first
    if derived.len() >= 4 {
        derived.remove(0);
    }
    derived.push((digest, Arc::clone(&d))); // alloc: Cold
    Ok(d)
}

fn io_of(io: &PartIoSpec) -> PartIoSpec {
    match io {
        PartIoSpec::Path(p) => PartIoSpec::Path(p.clone()),
        PartIoSpec::Mem { bytes, dev, ino } => PartIoSpec::Mem { bytes: Arc::clone(bytes), dev: *dev, ino: *ino },
    }
}

impl PartList {
    /// FASTADD: what the list's reader fills in place of the columns its parts lack (`None`: every part holds every
    /// column it reads).
    pub fn lacking(&self) -> Option<&pgrc_am::fastadd::Lacking> {
        self.lacking.as_deref()
    }

    /// Whether part `p` lacks the reader's column `attno` (its lane reads a stand-in).
    pub fn lacks(&self, p: usize, attno: u32) -> bool {
        self.lacking.as_deref().is_some_and(|l| l.lacks(p, attno))
    }

    /// The part column `attno` of part `p` is read from: `real` (part `p`), or the stand-in for a column it lacks.
    pub fn lane_part<'a>(&'a self, p: usize, attno: u32, real: &'a Arc<OpenPart>) -> PgResult<&'a Arc<OpenPart>> {
        match self.lacking.as_deref() {
            None => Ok(real),
            Some(l) => l.lane(p, attno, real),
        }
    }

    /// A key over part `p`'s content (its uuid words) as this list reads it: unchanged unless the part lacks a column,
    /// whose stand-in's fill then mixes in (the condition cache's unit).
    pub fn content_key(&self, p: usize, unit: [u64; 2]) -> [u64; 2] {
        match self.lacking.as_deref().filter(|l| l.lacks_any(p)) {
            Some(l) => [unit[0] ^ l.digest(), unit[1].rotate_left(17) ^ l.digest()],
            None => unit,
        }
    }
}
