//! FASTADD (pgrc-plans/FASTADD.done): ALTER TABLE ... ADD COLUMN (and DROP COLUMN) is metadata-only, as on heap: the
//! parts written before it keep their columns, the manifest lists them under the schema they were written with
//! (`pgrc_format::manifest::EXT_SCHEMAS`), and every reader fills a column a part lacks with the column's
//! `attmissingval` (PostgreSQL's rule, through the relcache; NULL without one).
//!
//! - The schema book: every older schema a generation describes, by fingerprint, harvested when the generation enters
//!   the process ([`harvest`], from `catalog::Generation`). A reader whose snapshot holds the generation before the ALTER
//!   (its header is the old schema and it describes nothing) reads its parts under the relation's new descriptor
//!   through the book: the ALTER's generation described the old schema. The book only grows; a fingerprint names one
//!   column list.
//! - [`Presence`]: which of a reader's columns a part holds, from the part's schema fingerprint (its footer's, checked
//!   at open): every column when it is the reader's; else the book's column list for it, each reader column present
//!   when the part's schema has its attno with the same storage, absent when it has no such attno. A schema the book
//!   does not know, or a column whose storage differs, is the typed refusal it always was (`OpenMismatch`, XX001).
//! - [`Fill`]: what a reader stores for an absent column.
//! - [`Lacking`]: for a reader that decodes stored streams only (Gravity), each part's absent columns as stand-in
//!   parts: one column of the fill at the part's granule geometry, sealed in memory once per (part, column) and read
//!   through the same cursors, sections and faces as a stored column.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use pgrc_format::class::ColSchema;
use pgrc_format::manifest::{schema_entries, Manifest, MANIFESTF_SCHEMAS};
use pgrc_read::{MemPartIo, OpenPart, PartExpect, ReadError};
use pgrc_write::elect::{CandidateSource, CodecCandidates};
use pgrc_write::ingest::{ColBuffer, RawDatum};
use pgsync::OnceLock;
use types_error::{PgError, PgResult};

pgsync::process_global! {
    static BOOK: pgsync::Mutex<BTreeMap<u64, Arc<[ColSchema]>>> = pgsync::Mutex::new(BTreeMap::new());
}

/// Book the older schemas `m` describes (none without the flag: no work).
pub(crate) fn harvest(m: &Manifest) -> pgrc_format::FormatResult<()> {
    if m.header.flags & MANIFESTF_SCHEMAS == 0 {
        return Ok(());
    }
    let entries = schema_entries(&m.ext)?;
    let mut book = pgsync::lock(&BOOK);
    for e in entries {
        book.entry(e.fingerprint).or_insert_with(|| Arc::from(e.cols)); // alloc: Cold
    }
    Ok(())
}

/// Book `cols` (a schema this process knows to be the table's: the ALTER's old descriptor).
pub fn note(cols: &[ColSchema]) {
    let fp = pgrc_format::ident::schema_fingerprint(cols);
    pgsync::lock(&BOOK).entry(fp).or_insert_with(|| Arc::from(cols)); // alloc: Cold
}

/// The column list of the schema `fingerprint`, when some generation described it.
pub fn schema_of(fingerprint: u64) -> Option<Arc<[ColSchema]>> {
    pgsync::lock(&BOOK).get(&fingerprint).cloned()
}

/// Whether a generation of `rel` a snapshot can still read lists parts under an older schema, or is of another schema
/// than the relation's (a reader of stored streams only, SQE, declines the table: `pgrc_front::served_lock`).
pub fn lagged(rel: &types_rel::Relation<'_>) -> PgResult<bool> {
    let locator = rel.rd_locator.get();
    let slot: crate::catalog::RelSlot = (locator.spcOid, locator.dbOid, locator.relNumber as u64);
    let Some((gens, _)) = crate::catalog::generations(slot) else { return Ok(false) };
    if gens.iter().any(|g| g.lagged()) {
        return Ok(true);
    }
    let fp = pgrc_format::ident::schema_fingerprint(&crate::schema::col_schemas(rel)?);
    Ok(gens.iter().any(|g| g.schema_fp != fp))
}

fn mismatch() -> Box<PgError> {
    crate::read_error(ReadError::OpenMismatch { field: "manifest schema_fingerprint" })
}

/// Which of a reader's columns (`reader`, fingerprint `reader_fp`) a part of schema `part_fp` holds: `None` = every
/// one; else one flag per reader column (`true` = present). Refuses a schema the book does not know and a column
/// whose storage differs.
pub fn presence(reader: &[ColSchema], reader_fp: u64, part_fp: u64) -> PgResult<Option<Arc<[bool]>>> {
    if part_fp == reader_fp {
        return Ok(None);
    }
    let theirs = schema_of(part_fp).ok_or_else(mismatch)?;
    let mut out = crate::alloc::try_vec(reader.len(), "fastadd")?; // alloc: Cold
    for c in reader {
        match theirs.binary_search_by_key(&c.attno, |t| t.attno) {
            Ok(i) if theirs[i].same_storage(c) => out.push(true),
            Ok(_) => return Err(mismatch()),
            Err(_) => out.push(false),
        }
    }
    Ok(if out.iter().all(|&p| p) { None } else { Some(Arc::from(out)) })
}

/// [`presence`] per part fingerprint, computed once per distinct fingerprint (a scan's parts share few).
pub struct Presences<'a> {
    reader: &'a [ColSchema],
    reader_fp: u64,
    seen: Vec<(u64, Option<Arc<[bool]>>)>,
}

impl<'a> Presences<'a> {
    pub fn new(reader: &'a [ColSchema], reader_fp: u64) -> Presences<'a> {
        Presences { reader, reader_fp, seen: Vec::new() }
    }

    /// The reader's schema fingerprint.
    pub fn reader_fp(&self) -> u64 {
        self.reader_fp
    }

    /// The reader's columns a part of schema `part_fp` holds ([`presence`]).
    pub fn of(&mut self, part_fp: u64) -> PgResult<Option<Arc<[bool]>>> {
        if part_fp == self.reader_fp {
            return Ok(None);
        }
        if let Some((_, p)) = self.seen.iter().find(|(f, _)| *f == part_fp) {
            return Ok(p.clone());
        }
        let p = presence(self.reader, self.reader_fp, part_fp)?;
        self.seen.push((part_fp, p.clone())); // alloc: Cold
        Ok(p)
    }

    /// Whether a part read so far lacks a column.
    pub fn any_absent(&self) -> bool {
        self.seen.iter().any(|(_, p)| p.is_some())
    }
}

/// What a reader stores for a column a part lacks: its `attmissingval` (a by-value word, or a by-reference image
/// copied out of the relcache, so it outlives a relcache rebuild), NULL without one.
#[derive(Clone, Debug)]
pub enum Fill {
    Null,
    Word(u64),
    /// The image (8-aligned words) and its length in bytes.
    Image(Arc<[u64]>, usize),
}

impl Fill {
    /// The datum word (a by-reference image's address), `None` = NULL.
    #[inline]
    pub fn word(&self) -> Option<u64> {
        match self {
            Fill::Null => None,
            Fill::Word(w) => Some(*w),
            Fill::Image(img, _) => Some(img.as_ptr() as u64),
        }
    }

    /// The image the word addresses, for a holder that must keep it alive while datums alias it.
    #[inline]
    pub fn image(&self) -> Option<&Arc<[u64]>> {
        match self {
            Fill::Image(img, _) => Some(img),
            _ => None,
        }
    }

    /// The by-reference image's bytes.
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            // SAFETY: the words hold at least `len` bytes (`fill_of`).
            Fill::Image(img, len) => Some(unsafe { core::slice::from_raw_parts(img.as_ptr() as *const u8, *len) }),
            _ => None,
        }
    }

    /// The writer's datum for it.
    fn raw(&self) -> RawDatum<'_> {
        match self {
            Fill::Null => RawDatum::Null,
            Fill::Word(w) => RawDatum::Word(*w),
            Fill::Image(..) => RawDatum::Bytes(self.bytes().expect("image")),
        }
    }

    /// The same fill: both NULL, the same word, or images of the same bytes.
    fn same(&self, o: &Fill) -> bool {
        match (self, o) {
            (Fill::Null, Fill::Null) => true,
            (Fill::Word(a), Fill::Word(b)) => a == b,
            (Fill::Image(..), Fill::Image(..)) => self.bytes() == o.bytes(),
            _ => false,
        }
    }

    /// Fold its identity into `h` (FNV-1a): NULL, a word, or an image's bytes.
    fn digest(&self, h: u64) -> u64 {
        match self {
            Fill::Null => fnv(h, &[0]),
            Fill::Word(w) => fnv(fnv(h, &[1]), &w.to_le_bytes()),
            Fill::Image(..) => fnv(fnv(h, &[2]), self.bytes().expect("image")),
        }
    }
}

fn fnv(h: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(h, |h, &b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

/// The fill of attribute `attno` (1-based) of `desc`: its missing value when it has one.
pub fn fill_of(desc: &types_tuple::tupdesc::TupleDescData<'_>, attno: u32) -> PgResult<Fill> {
    let i = attno as usize - 1;
    let Some(att) = desc.attrs.get(i) else { return Ok(Fill::Null) };
    if !att.atthasmissing || att.attisdropped {
        return Ok(Fill::Null);
    }
    let Some(m) = desc.constr.as_ref().and_then(|c| c.missing.get(i)).filter(|m| m.am_present) else {
        return Ok(Fill::Null);
    };
    if att.attbyval {
        return Ok(Fill::Word(m.am_value.as_u64()));
    }
    let p = m.am_value.as_u64() as *const u8;
    // SAFETY: a present by-reference attmissingval addresses its image in the relcache entry (varlena: a plain or
    // short header, never toasted in pg_attribute's array; cstring: NUL-terminated; else attlen bytes).
    let len = unsafe {
        match att.attlen {
            -1 => types_tuple::varatt::varsize_any(p),
            -2 => core::ffi::CStr::from_ptr(p as *const core::ffi::c_char).to_bytes_with_nul().len(),
            n => n as usize,
        }
    };
    let mut img = crate::alloc::try_filled(len.div_ceil(8), 0u64, "fastadd")?.into_boxed_slice(); // alloc: Cold
    // SAFETY: `img` holds at least `len` bytes; the source is `len` bytes (above).
    unsafe { core::ptr::copy_nonoverlapping(p, img.as_mut_ptr() as *mut u8, len) };
    Ok(Fill::Image(Arc::from(img), len))
}

/// The fills of `schemas`' columns under `desc` (one per column, schema order).
pub fn fills(desc: &types_tuple::tupdesc::TupleDescData<'_>, schemas: &[ColSchema]) -> PgResult<Vec<Fill>> {
    let mut out = crate::alloc::try_vec(schemas.len(), "fastadd")?;
    for s in schemas {
        out.push(fill_of(desc, s.attno)?);
    }
    Ok(out)
}

/// Stand-in parts' device number (`MemPartIo`): no file system's, so nothing keyed by a part's identity (its uuid:
/// device, inode, length, footer) takes a stand-in for a file or for another stand-in.
const STAND_IN_DEV: u64 = 0x4641_5354_414e_4449; // "FASTANDI"
pgsync::process_global! {
    static STAND_IN_INO: AtomicU64 = AtomicU64::new(1);
}

/// What a reader of stored streams fills, for one relation descriptor (the reader's) over one part list: per part,
/// the reader's columns it lacks, each read from a stand-in part ([`Lacking::lane`]). `None` from [`Lacking::of`] when
/// every part holds every column (one schema, the reader's: the common case).
pub struct Lacking {
    cols: Vec<ColSchema>,
    fills: Vec<Fill>,
    /// Per part (list order): the reader columns it lacks (indices into `cols`, ascending), each one's stand-in once
    /// built.
    absent: Vec<Box<[(u16, OnceLock<Arc<OpenPart>>)]>>,
    /// Per column: `(atttypid, atttypmod)`.
    types: Vec<(u32, i32)>,
    cands: CodecCandidates,
    rel: (u32, u32, u64),
    digest: u64,
}

impl Lacking {
    /// `rel`'s columns (the reader's descriptor) the parts of schemas `part_fps` (list order; `None`: the reader's)
    /// lack, with their fills from `rel`'s `attmissingval`.
    pub fn of(rel: &types_rel::Relation<'_>, part_fps: &[Option<u64>]) -> PgResult<Option<Lacking>> {
        let cols = crate::schema::col_schemas(rel)?;
        let fp = pgrc_format::ident::schema_fingerprint(&cols);
        if part_fps.iter().all(|f| f.is_none_or(|f| f == fp)) {
            return Ok(None);
        }
        let mut absent = crate::alloc::try_vec(part_fps.len(), "fastadd")?; // alloc: Cold
        {
            let mut pres = Presences::new(&cols, fp);
            for f in part_fps {
                // D2189: a stamp a column dropped with no ALTER publish left (`schema::book_dropped_stamp`)
                if let Some(f) = f.filter(|&f| f != fp) {
                    crate::schema::book_dropped_stamp(rel, f)?;
                }
                let lacks: Box<[(u16, OnceLock<Arc<OpenPart>>)]> = match f.map(|f| pres.of(f)).transpose()?.flatten() {
                    None => Box::new([]),
                    Some(p) => p.iter().enumerate().filter(|(_, &x)| !x).map(|(i, _)| (i as u16, OnceLock::new())).collect(), // alloc: Cold
                };
                absent.push(lacks);
            }
        }
        if absent.iter().all(|a| a.is_empty()) {
            return Ok(None);
        }
        let fills = fills(&rel.rd_att, &cols)?;
        let digest = fills.iter().fold(fnv(0xcbf2_9ce4_8422_2325, &fp.to_le_bytes()), |h, f| f.digest(h));
        let types = cols.iter().map(|c| rel.rd_att.attrs.get(c.attno as usize - 1).map_or((0, -1), |a| (a.atttypid, a.atttypmod))).collect(); // alloc: Cold
        let l = rel.rd_locator.get();
        Ok(Some(Lacking { cols, fills, absent, types, cands: crate::ingest::codec_candidates(rel)?, rel: (l.spcOid, l.dbOid, l.relNumber as u64), digest }))
    }

    /// The identity of what the reader reads in place of the absent columns (its schema and their fills): a cache
    /// keyed by a part's content mixes it in.
    pub fn digest(&self) -> u64 {
        self.digest
    }

    /// Whether part `p` lacks a reader column.
    pub fn lacks_any(&self, p: usize) -> bool {
        self.absent.get(p).is_some_and(|a| !a.is_empty())
    }

    /// Whether part `p` lacks the reader's column `attno`.
    pub fn lacks(&self, p: usize, attno: u32) -> bool {
        self.slot(p, attno).is_some()
    }

    fn slot(&self, p: usize, attno: u32) -> Option<&(u16, OnceLock<Arc<OpenPart>>)> {
        self.absent.get(p)?.iter().find(|(c, _)| self.cols[*c as usize].attno == attno)
    }

    /// The part that holds column `attno` of part `p` for the reader: `real` (part `p` itself), or the stand-in for a
    /// column it lacks, sealed once at `real`'s geometry (its rows, grain and granules) with every row the fill.
    pub fn lane<'a>(&'a self, p: usize, attno: u32, real: &'a Arc<OpenPart>) -> PgResult<&'a Arc<OpenPart>> {
        let Some((c, cell)) = self.slot(p, attno) else { return Ok(real) };
        if let Some(s) = cell.get() {
            return Ok(s);
        }
        let built = self.stand_in(real, *c as usize)?;
        let _ = cell.set(built);
        Ok(cell.get().expect("set"))
    }

    fn stand_in(&self, real: &OpenPart, c: usize) -> PgResult<Arc<OpenPart>> {
        let col = &self.cols[c];
        let (posture, sample) = self.cands.election_inputs(col.attno, 0);
        let key = StandInKey { rows: real.rows(), grain: real.grain(), col: *col, ty: self.types[c], fill: self.fills[c].clone(), posture, sample };
        let part = STAND_INS.get_or_seal((self.rel, real.uuid(), col.attno), key, || seal_stand_in(col, &self.fills[c], &self.cands, self.rel, real.rows(), real.grain()))?;
        if part.footer().granule_count != real.footer().granule_count {
            return Err(Box::new(PgError::error(format!("pgrcolumnar: the stand-in of column {} has another geometry than its part", col.attno))));
        }
        Ok(part)
    }
}

/// The byte budget of the process's stand-ins ([`StandIns`]).
pub const STAND_IN_CACHE_BYTES: usize = 256 << 20;
/// What an entry costs beyond its image (its open part's decoded footer and directory, the entry).
const STAND_IN_ENTRY_OVERHEAD: usize = 4 << 10;

/// A stand-in's place: the relation's slot, the part's uuid, the column's attno.
type StandInPlace = ((u32, u32, u64), [u8; 16], u32);

/// Everything else a stand-in's bytes are a function of. A later ALTER cannot alias an entry: a re-added column has
/// another attno, a retype rewrites into other parts, SET DEFAULT leaves `attmissingval`, a reloption changes the
/// posture.
#[derive(Clone)]
struct StandInKey {
    rows: u64,
    grain: pgrc_format::geom::GranuleGrain,
    col: ColSchema,
    ty: (u32, i32),
    fill: Fill,
    posture: pgrc_write::elect::ColumnPosture,
    sample: pgrc_codec::election::SampleFamilies,
}

impl StandInKey {
    fn same(&self, o: &StandInKey) -> bool {
        self.rows == o.rows && self.grain == o.grain && self.col == o.col && self.ty == o.ty && self.posture == o.posture && self.sample == o.sample && self.fill.same(&o.fill)
    }
}

struct StandInEntry {
    key: StandInKey,
    part: Arc<OpenPart>,
    bytes: usize,
    used: u64,
}

struct StandInState {
    map: BTreeMap<StandInPlace, Vec<StandInEntry>>,
    bytes: usize,
    tick: u64,
}

/// Counters of the process's stand-ins: `(sealed, reused, evicted, entries, bytes)`.
pub fn stand_in_stats() -> (u64, u64, u64, usize, usize) {
    STAND_INS.stats()
}

/// The stand-ins of every reader, across generations: a publish to a lagged table makes a new part list whose
/// [`Lacking`] finds its old parts' stand-ins here instead of sealing them again. Least recently used out past `cap`
/// bytes; a slot's entries go when the catalog removes it (DROP, TRUNCATE, rewrite).
struct StandIns {
    state: pgsync::Mutex<StandInState>,
    cap: usize,
    sealed: AtomicU64,
    reused: AtomicU64,
    evicted: AtomicU64,
}

pgsync::process_global! {
    static STAND_INS: StandIns = StandIns::new(STAND_IN_CACHE_BYTES);
    static STAND_IN_LISTENING: AtomicBool = AtomicBool::new(false);
}

impl StandIns {
    const fn new(cap: usize) -> StandIns {
        StandIns { state: pgsync::Mutex::new(StandInState { map: BTreeMap::new(), bytes: 0, tick: 0 }), cap, sealed: AtomicU64::new(0), reused: AtomicU64::new(0), evicted: AtomicU64::new(0) }
    }

    fn find(&self, place: &StandInPlace, key: &StandInKey) -> Option<Arc<OpenPart>> {
        let mut st = pgsync::lock(&self.state);
        st.tick += 1;
        let tick = st.tick;
        let e = st.map.get_mut(place)?.iter_mut().find(|e| e.key.same(key))?;
        e.used = tick;
        Some(Arc::clone(&e.part))
    }

    fn get_or_seal(&self, place: StandInPlace, key: StandInKey, seal: impl FnOnce() -> PgResult<(Arc<OpenPart>, usize)>) -> PgResult<Arc<OpenPart>> {
        if !STAND_IN_LISTENING.load(Ordering::Acquire) && !STAND_IN_LISTENING.swap(true, Ordering::AcqRel) {
            crate::catalog::on_slot_event(stand_in_slot_event);
        }
        if let Some(p) = self.find(&place, &key) {
            self.reused.fetch_add(1, Ordering::Relaxed);
            return Ok(p);
        }
        let (part, image) = seal()?;
        self.sealed.fetch_add(1, Ordering::Relaxed);
        let bytes = image + STAND_IN_ENTRY_OVERHEAD;
        if bytes > self.cap / 4 {
            return Ok(part);
        }
        let mut st = pgsync::lock(&self.state);
        st.tick += 1;
        let used = st.tick;
        let es = st.map.entry(place).or_default();
        if let Some(e) = es.iter_mut().find(|e| e.key.same(&key)) {
            e.used = used;
            return Ok(Arc::clone(&e.part));
        }
        es.push(StandInEntry { key, part: Arc::clone(&part), bytes, used }); // alloc: Cold
        st.bytes += bytes;
        if st.bytes > self.cap {
            self.evict(&mut st, self.cap - self.cap / 8);
        }
        Ok(part)
    }

    /// Least recently used first, down to `to` bytes.
    fn evict(&self, st: &mut StandInState, to: usize) {
        let mut by_use: Vec<(u64, StandInPlace)> = st.map.iter().flat_map(|(p, es)| es.iter().map(move |e| (e.used, *p))).collect(); // alloc: Cold
        by_use.sort_unstable();
        for (used, place) in by_use {
            if st.bytes <= to {
                break;
            }
            let Some(es) = st.map.get_mut(&place) else { continue };
            let Some(i) = es.iter().position(|e| e.used == used) else { continue };
            st.bytes -= es.swap_remove(i).bytes;
            if es.is_empty() {
                st.map.remove(&place);
            }
            self.evicted.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn remove_slot(&self, slot: (u32, u32, u64)) {
        let mut st = pgsync::lock(&self.state);
        let places: Vec<StandInPlace> = st.map.range((slot, [0; 16], 0)..=(slot, [u8::MAX; 16], u32::MAX)).map(|(p, _)| *p).collect(); // alloc: Cold
        for p in places {
            if let Some(es) = st.map.remove(&p) {
                st.bytes -= es.iter().map(|e| e.bytes).sum::<usize>();
                self.evicted.fetch_add(es.len() as u64, Ordering::Relaxed);
            }
        }
    }

    fn stats(&self) -> (u64, u64, u64, usize, usize) {
        let st = pgsync::lock(&self.state);
        (self.sealed.load(Ordering::Relaxed), self.reused.load(Ordering::Relaxed), self.evicted.load(Ordering::Relaxed), st.map.values().map(Vec::len).sum(), st.bytes)
    }
}

fn stand_in_slot_event(ev: &crate::catalog::SlotEvent<'_>) {
    use crate::catalog::SlotEvent;
    if let SlotEvent::Removed(slot, _) | SlotEvent::Created(slot, _) = *ev {
        STAND_INS.remove_slot(slot);
    }
}

/// A stand-in part: column `col` alone, `rows` rows each `fill`, at `grain`, sealed in memory with `cands` (the
/// relation's codec candidates; `rel` its `(spc, db, relfilenumber)`) and opened over a private identity; and its
/// image's bytes.
fn seal_stand_in(col: &ColSchema, fill: &Fill, cands: &CodecCandidates, rel: (u32, u32, u64), rows: u64, grain: pgrc_format::geom::GranuleGrain) -> PgResult<(Arc<OpenPart>, usize)> {
    let mut buf = ColBuffer::new(*col);
    buf.append_repeated(fill.raw(), rows).map_err(crate::write_error)?;
    let spec = pgrc_write::seal::PartSpec { spc: rel.0, db: rel.1, relfilenumber: rel.2, schema_fingerprint: pgrc_format::ident::schema_fingerprint(&[*col]) };
    let sources: [&dyn CandidateSource; 1] = [cands];
    let bytes = pgrc_write::seal::seal_part_image(&spec, &[buf], &sources, &pgrc_write::seal::CodecResolver, grain).map_err(crate::write_error)?;
    let image = bytes.len();
    let ino = STAND_IN_INO.fetch_add(1, Ordering::Relaxed);
    let part = OpenPart::open(Box::new(MemPartIo::new(bytes, STAND_IN_DEV, ino)), &PartExpect::none()).map_err(crate::read_error)?; // alloc: Cold
    if part.rows() != rows || part.grain() != grain {
        return Err(Box::new(PgError::error(format!("pgrcolumnar: the stand-in of column {} has another geometry than its part", col.attno))));
    }
    Ok((Arc::new(part), image)) // alloc: Cold
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgrc_format::class::{CollationClass, StorageClass};

    fn int8(attno: u32) -> ColSchema {
        ColSchema::opaque(attno, StorageClass::ByvalWord { width: 8, signed: true }, 8, true, b'd', CollationClass::C)
    }

    fn text(attno: u32) -> ColSchema {
        ColSchema::opaque(attno, StorageClass::VarlenaVerbatim, -1, false, b'i', CollationClass::C)
    }

    #[test]
    fn presence_follows_the_booked_schema_and_refuses_what_it_cannot_describe() {
        let (old, new) = (vec![int8(1), text(2)], vec![int8(1), text(2), int8(4)]);
        let (fo, fnew) = (pgrc_format::ident::schema_fingerprint(&old), pgrc_format::ident::schema_fingerprint(&new));
        assert!(presence(&new, fnew, fnew).unwrap().is_none(), "the reader's own schema: every column");
        assert!(presence(&new, fnew, fo ^ 0x55).is_err(), "an unknown schema refuses");
        note(&old);
        assert_eq!(schema_of(fo).as_deref(), Some(&old[..]));
        assert_eq!(presence(&new, fnew, fo).unwrap().as_deref(), Some(&[true, true, false][..]));
        // a dropped column (the part has attno 2, the reader not): every reader column present
        let dropped = vec![int8(1)];
        assert!(presence(&dropped, pgrc_format::ident::schema_fingerprint(&dropped), fo).unwrap().is_none());
        // the same attno with another storage refuses
        let retyped = vec![int8(1), int8(2)];
        assert!(presence(&retyped, pgrc_format::ident::schema_fingerprint(&retyped), fo).is_err());
        let mut p = Presences::new(&new, fnew);
        assert!(p.of(fnew).unwrap().is_none() && !p.any_absent());
        assert!(p.of(fo).unwrap().is_some() && p.any_absent());
    }

    /// The datums of every granule of `part`'s column `attno` (`None`: NULL), through the server's binding.
    fn decode_all(part: &Arc<OpenPart>, attno: u32, varlena: bool) -> Vec<Option<Vec<u8>>> {
        use pgrc_format::abi::{ByteArena, DecodeOut, ValidityVerdict};
        let mut cur = pgrc_read::StreamCursor::open(Arc::clone(part), crate::scan::codec_binding(), attno, 0).expect("cursor");
        let mut out = Vec::new();
        for g in 0..cur.granule_count() {
            let n = cur.rows_in_granule(g) as usize;
            let mut datums = vec![0u64; n];
            let mut arena = vec![0u64; (n * 320).div_ceil(8) + 64];
            // SAFETY: an 8-aligned word buffer viewed as bytes, written by the decode alone.
            let bytes = unsafe { core::slice::from_raw_parts_mut(arena.as_mut_ptr() as *mut u8, arena.len() * 8) };
            let got = cur.decode_full(g, &mut DecodeOut { datums: &mut datums, arena: ByteArena::new(bytes) }).expect("decode");
            assert_eq!(got as usize, n);
            let mut valid = vec![0u64; n.div_ceil(64)];
            let all = !cur.has_validity_stream() || matches!(cur.validity(g, &mut valid).expect("validity"), ValidityVerdict::AllValid);
            for (r, &d) in datums.iter().enumerate() {
                if !all && valid[r / 64] >> (r % 64) & 1 == 0 {
                    out.push(None);
                } else if varlena {
                    // SAFETY: a decoded varlena datum addresses its image (4-byte header) in the arena or the dictionary.
                    let len = unsafe { types_tuple::varatt::varsize_any(d as *const u8) };
                    out.push(Some(unsafe { core::slice::from_raw_parts(d as *const u8, len) }.to_vec()));
                } else {
                    out.push(Some(d.to_le_bytes().to_vec()));
                }
            }
        }
        out
    }

    /// FASTADD's stand-in part (Gravity's lanes over a column a part lacks): the forced grain's geometry for any row
    /// count, every row the fill (a word, a long text image stored once in the buffer, NULL), under the storage posture
    /// and under the text dictionary posture the relation's candidates give a text column.
    #[test]
    fn a_stand_in_part_has_its_parts_geometry_and_every_row_the_fill() {
        use pgrc_format::geom::{granule_count_at, GranuleGrain};
        use pgrc_write::elect::{ColumnPosture, DictPolicy};
        let storage = ColumnPosture { fused: false, cold: true, numeric: false, dict: None };
        let dict = ColumnPosture { dict: Some(DictPolicy { ndv_cap: 1 << 20, exec_ok: true, sem: pgrc_write::dict::TextSemantics::BytesOnly }), ..storage };
        let mut long = pgrc_format::wire::varlena_header_4b_u(300).to_le_bytes().to_vec();
        long.extend_from_slice(&[b'L'; 300]);
        let img: Vec<u64> = long.chunks(8).map(|c| { let mut w = [0u8; 8]; w[..c.len()].copy_from_slice(c); u64::from_le_bytes(w) }).collect();
        let long_fill = Fill::Image(Arc::from(img), long.len());
        for (rows, grain) in [(20_000u64, 8192u32), (20_000, 1024), (1, 2048), (8192, 8192)] {
            let grain = GranuleGrain::from_rows(grain).unwrap();
            let cases: [(ColSchema, &Fill, ColumnPosture, Option<Vec<u8>>); 4] = [
                (int8(4), &Fill::Word(7), storage, Some(7u64.to_le_bytes().to_vec())),
                (int8(5), &Fill::Null, storage, None),
                (text(6), &long_fill, storage, Some(long.clone())),
                (text(6), &long_fill, dict, Some(long.clone())),
            ];
            for (col, fill, posture, want) in cases {
                let cands = CodecCandidates::new(posture);
                let (part, _) = seal_stand_in(&col, fill, &cands, (1663, 5, 900), rows, grain).expect("stand-in");
                assert_eq!((part.rows(), part.grain(), part.footer().granule_count as u64), (rows, grain, granule_count_at(rows, grain)), "{col:?} at {grain:?}");
                let got = decode_all(&part, col.attno, col.class == pgrc_format::class::StorageClass::VarlenaVerbatim);
                assert_eq!(got.len() as u64, rows);
                assert!(got.iter().all(|d| *d == want), "every row the fill: {col:?} {posture:?}");
            }
        }
        let mut b = ColBuffer::new(text(6));
        b.append_repeated(long_fill.raw(), 100_000).unwrap();
        assert!(b.heap_len() < 1024, "the image is stored once, not per row: {}", b.heap_len());
    }

    fn storage() -> pgrc_write::elect::ColumnPosture {
        pgrc_write::elect::ColumnPosture { fused: false, cold: true, numeric: false, dict: None }
    }

    /// A list's view of `reals` for a reader whose column `col` (fill `fill`) every part lacks.
    fn lacking(rel: (u32, u32, u64), col: ColSchema, fill: Fill, nparts: usize) -> Lacking {
        Lacking {
            cols: vec![int8(1), col],
            fills: vec![Fill::Null, fill],
            absent: (0..nparts).map(|_| vec![(1u16, OnceLock::new())].into_boxed_slice()).collect(),
            types: vec![(20, -1), (20, -1)],
            cands: CodecCandidates::new(storage()),
            rel,
            digest: 0,
        }
    }

    /// FASTADD (coordinator 09-01 decision b): a publish to a lagged table makes a new part list, whose stand-ins for
    /// the old parts are the ones the previous list sealed; another fill, attno, part or relation seals its own; a
    /// removed slot's entries go.
    #[test]
    fn stand_ins_outlive_a_generation_and_key_by_part_attno_and_fill() {
        use pgrc_format::geom::GranuleGrain;
        let grain = GranuleGrain::from_rows(8192).unwrap();
        let cands = CodecCandidates::new(storage());
        let reals: Vec<Arc<OpenPart>> = [20_000u64, 9_000].iter().map(|&n| seal_stand_in(&int8(1), &Fill::Word(1), &cands, (1663, 5, 991_700), n, grain).unwrap().0).collect();
        let rel = (1663, 5, 991_701);
        let (sealed0, reused0, ..) = stand_in_stats();
        let gen1 = lacking(rel, int8(4), Fill::Word(7), 2);
        let s0 = Arc::clone(gen1.lane(0, 4, &reals[0]).unwrap());
        let s1 = Arc::clone(gen1.lane(1, 4, &reals[1]).unwrap());
        assert!(!Arc::ptr_eq(&s0, &s1), "two parts, two stand-ins");
        assert_eq!((s0.rows(), s1.rows()), (20_000, 9_000));
        assert!(Arc::ptr_eq(gen1.lane(0, 4, &reals[0]).unwrap(), &s0));
        let gen2 = lacking(rel, int8(4), Fill::Word(7), 2);
        assert!(Arc::ptr_eq(gen2.lane(0, 4, &reals[0]).unwrap(), &s0), "the next generation reuses part 0's stand-in");
        assert!(Arc::ptr_eq(gen2.lane(1, 4, &reals[1]).unwrap(), &s1), "and part 1's");
        let (sealed1, reused1, ..) = stand_in_stats();
        assert!(sealed1 - sealed0 >= 2 && reused1 - reused0 >= 2);
        for (what, other) in [
            ("another fill", lacking(rel, int8(4), Fill::Word(8), 2)),
            ("a NULL fill", lacking(rel, int8(4), Fill::Null, 2)),
            ("another attno", lacking(rel, int8(5), Fill::Word(7), 2)),
            ("another relation", lacking((1663, 5, 991_702), int8(4), Fill::Word(7), 2)),
        ] {
            let s = other.lane(0, other.cols[1].attno, &reals[0]).unwrap();
            assert!(!Arc::ptr_eq(s, &s0), "{what} seals its own stand-in");
        }
        let mut retyped = lacking(rel, int8(4), Fill::Word(7), 2);
        retyped.types[1] = (23, -1);
        assert!(!Arc::ptr_eq(retyped.lane(0, 4, &reals[0]).unwrap(), &s0), "another type seals its own stand-in");
        STAND_INS.remove_slot(rel);
        let gen3 = lacking(rel, int8(4), Fill::Word(7), 2);
        assert!(!Arc::ptr_eq(gen3.lane(0, 4, &reals[0]).unwrap(), &s0), "a removed slot's stand-ins are gone");
        stand_in_slot_event(&crate::catalog::SlotEvent::Removed(rel, "/nowhere"));
        let gen4 = lacking(rel, int8(4), Fill::Word(7), 2);
        assert!(!Arc::ptr_eq(gen4.lane(0, 4, &reals[0]).unwrap(), gen3.lane(0, 4, &reals[0]).unwrap()), "the catalog's Removed event drops them too");
    }

    /// The byte cap: least recently used out, a touched entry kept, the total under the cap.
    #[test]
    fn stand_ins_stay_under_their_byte_cap_least_recently_used_first() {
        let cands = CodecCandidates::new(storage());
        let grain = pgrc_format::geom::GranuleGrain::from_rows(8192).unwrap();
        let (part, _) = seal_stand_in(&int8(4), &Fill::Word(7), &cands, (1663, 5, 991_800), 100, grain).unwrap();
        let image = 1000;
        let cache = StandIns::new(8 * (image + STAND_IN_ENTRY_OVERHEAD));
        let key = |n: u64| StandInKey { rows: n, grain, col: int8(4), ty: (20, -1), fill: Fill::Word(7), posture: storage(), sample: Default::default() };
        let place = |n: u8| ((1663u32, 5u32, 991_800u64), [n; 16], 4u32);
        let mut seals = 0;
        for n in 0..6u8 {
            cache.get_or_seal(place(n), key(100), || { seals += 1; Ok((Arc::clone(&part), image)) }).unwrap();
        }
        cache.get_or_seal(place(0), key(100), || { seals += 1; Ok((Arc::clone(&part), image)) }).unwrap();
        assert_eq!(seals, 6, "place 0 found");
        for n in 6..12u8 {
            cache.get_or_seal(place(n), key(100), || { seals += 1; Ok((Arc::clone(&part), image)) }).unwrap();
        }
        let (_, _, evicted, entries, bytes) = cache.stats();
        assert!(bytes <= cache.cap && evicted >= 4 && entries <= 8, "under the cap: {bytes} bytes, {entries} entries, {evicted} evicted");
        assert!(cache.find(&place(0), &key(100)).is_some(), "the touched entry stays");
        assert!(cache.find(&place(1), &key(100)).is_none(), "the oldest untouched one went first");
        assert!(cache.find(&place(11), &key(100)).is_some());
        assert!(cache.find(&place(11), &key(101)).is_none(), "another geometry is another entry");
        let big = StandIns::new(4 * STAND_IN_ENTRY_OVERHEAD);
        big.get_or_seal(place(0), key(100), || Ok((Arc::clone(&part), 2 * STAND_IN_ENTRY_OVERHEAD))).unwrap();
        assert_eq!(big.stats().3, 0, "an entry over a quarter of the cap is not kept");
    }

    /// The bytes a stand-in holds at 1M rows (the FASTADD record's budget arithmetic): `x int DEFAULT 7` under the
    /// storage posture, `t text DEFAULT 'dflt'` under it and under the dictionary posture the server gives C-collated
    /// text, at the 8192-row grain.
    #[test]
    fn stand_in_bytes_at_a_million_rows() {
        use pgrc_write::elect::DictPolicy;
        let grain = pgrc_format::geom::GranuleGrain::from_rows(8192).unwrap();
        let int4 = ColSchema::opaque(5, StorageClass::ByvalWord { width: 4, signed: true }, 4, true, b'i', CollationClass::C);
        let mut dflt = pgrc_format::wire::varlena_header_4b_u(4).to_le_bytes().to_vec();
        dflt.extend_from_slice(b"dflt");
        let t = Fill::Image(Arc::from(vec![u64::from_le_bytes(dflt.try_into().unwrap())]), 8);
        let dict = pgrc_write::elect::ColumnPosture { dict: Some(DictPolicy { ndv_cap: 1 << 24, exec_ok: true, sem: pgrc_write::dict::TextSemantics::BytesOnly }), ..storage() };
        for (what, col, fill, posture, bound) in [
            ("x int DEFAULT 7", int4, Fill::Word(7), storage(), 256usize << 10),
            ("t text DEFAULT 'dflt' (storage)", text(6), t.clone(), storage(), 256 << 10),
            ("t text DEFAULT 'dflt' (dictionary)", text(6), t, dict, 256 << 10),
        ] {
            let (part, image) = seal_stand_in(&col, &fill, &CodecCandidates::new(posture), (1663, 5, 991_900), 1 << 20, grain).unwrap();
            assert_eq!(part.rows(), 1 << 20);
            eprintln!("FASTADD stand-in bytes at 1,048,576 rows: {what}: {image} B image + {STAND_IN_ENTRY_OVERHEAD} B entry");
            assert!(image <= bound, "{what}: {image} B");
        }
    }
}
