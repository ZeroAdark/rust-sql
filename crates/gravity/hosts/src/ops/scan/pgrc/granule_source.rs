//! Worker range source over pgrcolumnar granules (milestone-1b §1.9, §1.10;
//! partitioned-record.md §4.2): the leader resolves every granule relation of
//! the statement once into `Resolved` and hands it to every source through the
//! Binding; a source decodes int lanes granule by granule (int2 / int4 lanes
//! typed through `decode_i16` / `decode_i32`, published at their width) and
//! publishes each granule as one window; the per-part faces (the open part,
//! its Stats bodies, its dictionaries) are filled once per statement and shared.

use std::ffi::CString;
use std::sync::atomic::{AtomicU64, Ordering};
use std::collections::HashMap;
use std::sync::Arc;

use cancelpoll::Step;
use pgsync::{Mutex, OnceLock};

use gravity_abi::ctx::lane;
use gravity_abi::{Answer, Datum as Word, Lane, ScanPos, ANSWER_MINMAX};
use gravity_driver::{BindingRef, Cursor, Error, Residency, Result, Source, Window, WindowBase};
use gravity_program::{laneset, ZoneOp, ZonePred};
use gravity_program::cost::pgrc::LaneDecode;

use crate::inputs::knob_kb;
use crate::ops::scan::pgrc::lanebits::LaneBits;
use crate::ops::scan::heap_source::{HeldCharge, HeldOwner};

use pgrc_am::footer::ZoneColumn;
use pgrc_format::abi::{ByteArena, DecodeOut, Selection, ValidityVerdict};
use pgrc_format::class::StorageClass;
use pgrc_format::geom::{FRAMES_PER_GRANULE, FRAME_VALUES, GRANULE_ROWS};
use pgrc_format::meta::{KeyKind, Sortedness, StatsRecord, STATSF_COMPUTED, STATS_RECORD_LEN};
use pgrc_format::part::{SectionKind, StreamRole, SECTIONF_META_ZSTD};
use pgrc_format::shredlane::{numeric_lane_scale, ShredLaneKind};
use pgrc_format::wire::Cur;
use pgrc_format::enc::EncodingId;
use pgrc_format::meta::Verdict as MetaVerdict;
use pgrc_meta::lower::{lower_const, ConstInput, Lowering};
use pgrc_meta::profile::MetaProfile;
use pgrc_meta::verdict::{evaluate, BloomEvidence, GrainFacts, ZonePredicate};
use pgrc_meta::{bloom, psma};
use gravity_abi::site::{PackedFrames, LANE_PACKED, PACKED_FRAMES};
use gravity_program::protos::{code_sparse_table, code_table, const_key_lane, word_zone_at_of, word_zone_key_of, word_zone_list_of, word_zone_of, VERDICT_BLOOM, VERDICT_CLAIM, VERDICT_CODE_SPARSE, VERDICT_CODE_TABLE, VERDICT_COND, VERDICT_CONST_KEY, VERDICT_COUNT_ONLY, VERDICT_COUNT_SELECT, VERDICT_ELIDE, VERDICT_ELIDE_HIT, VERDICT_GUARD_SEL, VERDICT_LATE, VERDICT_PACKED, VERDICT_PACKED_ZONE, VERDICT_PSMA, VERDICT_UNREAD, VERDICT_ZONE};
use pgrc_read::{DictHandle, DictTail, DictTouch, MemPartIo, OpenPart, PartExpect, PartIo, PartPin, PartRegistry, ReadResult, StreamCursor, VfsPartIo};
use types_core::catalog::{NUMERICOID, RELPERSISTENCE_PERMANENT, RELPERSISTENCE_UNLOGGED};
use pgrc_format::class::TypeSemantics;
use pgrc_scan::{PartDeletes, DV_GRANULE_WORDS};
use tableam_vocab::TableAm;
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_rel::{Relation, RELKIND_MATVIEW, RELKIND_RELATION};
use types_snapshot::SnapshotData;

mod fastadd;
mod fetch;
mod held;
pub use held::{let_stale_faces_go, let_warm_go};
mod kept;
mod lanes;
mod late;
mod raw;
mod release;
pub use release::{part_cache_held, part_cache_mib, part_release_census};
mod sweep;
use self::sweep::{SharedSweep, StmtSweeps, SweepShare};
mod verdicts;

use self::raw::walk_sel;


/// The 4B-U varlena image of `''` (`EMPTY_PART`'s value).
static EMPTY_IMAGE: [u8; 4] = [16, 0, 0, 0];

/// The jsonb `{}` image (a 4-byte varlena header, length 8, then an object container of 0 pairs: `JB_FOBJECT`): the cell
/// `guard_fill` leaves in a row it does not fill. Every `->` / `->>` / `#>>` chain over it is NULL, `@>` of a non-empty
/// object template false, `?` false.
#[repr(C, align(8))]
struct ObjectImage([u8; 8]);
static EMPTY_OBJECT: ObjectImage = ObjectImage([32, 0, 0, 0, 0, 0, 0, 0x20]);

/// Whether an invalid row of a path lane a part elected reads as SQL NULL through every compare and key over that path:
/// the writer seals a part's lane only when no row holds another kind or a container at the path (the exception-freedom
/// law; a jsonb null is a lane NULL whose `->>` is SQL NULL too, and its lane seals without STREAMF_SHRED_OBJECT_ROOTS,
/// so a presence lane over it is invalid and fills), and it never descends an array; so such a row's path is absent, a
/// jsonb null, or crosses a non-object, and each `->>` chain or compare over it gives NULL, as over `{}`. Not so for a `#>>` element PostgreSQL may read as an array index
/// (jsonfuncs.c `get_path_all`: `strtoint` of the whole element), which an array at that level answers: a path holding
/// any segment of that form (split on every `.`, escapes ignored: more segments, never fewer) is not inferred.
pub(crate) fn null_when_absent(path: &str) -> bool {
    !path.split('.').any(|seg| {
        let t = seg.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '\u{b}');
        let d = t.strip_prefix(['+', '-']).unwrap_or(t);
        !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())
    })
}

pub const ROWS: usize = GRANULE_ROWS as usize;
const VALIDITY_WORDS: usize = ROWS / 64;
/// A part's choice under road dict.entries' runs (`GranuleRangeSource::part_runs`): its runs taken, or its rows.
const RUNS_TAKEN: u8 = 1;
const RUNS_ROWS: u8 = 2;
const NULLS_NONE: u8 = 1;
const NULLS_SOME: u8 = 2;
const NULLS_UNKNOWN: u8 = 3;
/// A lane's `PackedFrames` in `datums` words.
const PF_WORDS: usize = core::mem::size_of::<PackedFrames>() / 8;
const _: () = assert!(PACKED_FRAMES == FRAMES_PER_GRANULE as usize && core::mem::size_of::<PackedFrames>() % 8 == 0);

/// The datum words a `lw`-byte by-value lane holds, as i64 (`[lo, hi]`): sign-extended, or zero-extended for an unsigned
/// stream (oid: `StorageClass::ByvalWord { signed: false }`; an 8-byte one only up to i64::MAX), so a packed frame's
/// `fmax`, clamped to `hi`, bounds every value of the frame.
fn packed_words(lw: u32, unsigned: bool) -> (i64, i64) {
    match unsigned {
        false => (i64::MIN >> (64 - 8 * lw), i64::MAX >> (64 - 8 * lw)),
        true => (0, (u64::MAX >> (64 - 8 * lw)).min(i64::MAX as u64) as i64),
    }
}

pub enum PartIoSpec {
    Path(CString),
    Mem { bytes: Arc<[u8]>, dev: u64, ino: u64 },
}

pub struct PartSpec {
    pub io: PartIoSpec,
    pub expect: PartExpect,
    pub rows: u64,
    pub granules: u32,
    pub deletes: Option<Arc<PartDeletes>>,
    /// The part as the resident table holds it open (`residency.rs`): a source
    /// pins it without an open; `None` opens through the registry (the tests' in-memory parts).
    pub part: Option<Arc<OpenPart>>,
}

/// One execution's parts in manifest order; `starts[i]` is part i's first global granule.
pub struct PartList {
    /// D4552 (a): the list's own lists (`parts` with their paths, `starts`) and its `Arc` block (`arc_held`) on the part
    /// cache's holder from the build, as its faces are (`Faces::held`): first, so it drops before them, on whichever thread
    /// drops the list last (a statement's end, the part cache's evicted hook on another backend).
    held: memledger::Charge,
    /// D4537 4: the zone columns' bytes the list keeps (`set_zones`), on the part cache. D4552 (a): with the charges
    /// first, so it drops before `zones`: the bytes come back to the dropping thread before the columns free (a large
    /// relation's own columns cross door 1's fold step inside the drop).
    zones_charge: OnceLock<memledger::Charge>,
    parts: Vec<PartSpec>,
    starts: Vec<u64>,
    granules: u64,
    rows: u64,
    /// The parts' deleted rows (their delete vectors'), with `rows` and `granules` (SCANSTATS' denominators per bind).
    deleted: u64,
    generation: u64,
    /// The generation's part-grain answers as the catalog holds them (`None`: the tests'
    /// in-memory parts; the part records are read from the parts then, a declared plain read).
    answers: Option<Arc<pgrc_am::answers::Answers>>,
    faces: Faces,
    /// The relation footer's resident zone columns of every granule (attno, column), taken once per generation by the
    /// first `resolve` that holds the relation: a source's serial seek reads its zone census from them (no part opened).
    zones: OnceLock<Box<[(u32, Arc<ZoneColumn>)]>>,
    /// FASTADD (`granule_source/fastadd.rs`): the stand-ins of the columns its parts lack for its reader (a derived
    /// list's), and a base list's derived lists by what their stand-ins hold.
    lacking: Option<Arc<pgrc_am::fastadd::Lacking>>,
    derived: Mutex<Vec<(u64, Arc<PartList>)>>,
    /// D4410: the part cache's evicted hook let the list's warm dictionary cells go (`held::let_warm_go`): it keeps none
    /// from now on (its statements hold their own).
    warm_off: pgsync::atomic::AtomicBool,
    /// D4410: the list is in the evicted hook's walk (`held::register`, at its first install).
    registered: pgsync::atomic::AtomicBool,
}

/// The column faces: attnos `1..=MaxHeapAttributeNumber` (D1748: every column a table can have; face 0 unused), the shred
/// faces above them.
pub const MAX_FACE_ATTNO: usize = types_tuple::htup::MaxHeapAttributeNumber as usize + 1;
/// The shredded jsonb path lanes a list's faces hold beside its columns (M6 Decision 8c, R13-L03 / L04): one face per
/// distinct lane `(attno, path, kind, scale)` its statements read, at `MAX_FACE_ATTNO + slot`, claimed on first use.
pub const MAX_SHRED_FACES: usize = gravity_program::cost::pgrc::SHRED_FACES_MAX;
// D1748: a shred face is never a column's (every attno below MAX_FACE_ATTNO), and every face fits a stream key's 16 bits
const _: () = assert!(MAX_FACE_ATTNO > types_tuple::htup::MaxHeapAttributeNumber as usize && MAX_FACE_ATTNO + MAX_SHRED_FACES <= 1 << 16);

/// The read-only faces of a list's parts (Stats faces, bodies, dictionaries per (face, part)), resident with the list for
/// the generation (R2, §4b (d)). A face is a column's attno (its `path_ord` 0 streams) or a shred lane's slot (its path's
/// streams at the `path_ord` each part elected it at, `Faces::locate`): a path lane never reads its column's faces.
pub struct Faces {
    /// D4482: the part cache's charge for the faces the list keeps for its generation: its face tables, each lane's
    /// tables and every Stats / bloom / PSMA body read into them (`hold`), on the thread that read it. First, so it drops
    /// before them.
    held: Mutex<memledger::Charge>,
    lanes: Box<[OnceLock<LaneFaces>]>,
    shred: Box<[OnceLock<ShredFace>]>,
    nparts: usize,
}

/// A shred lane's face: its identity and, per part, the `path_ord` the part elected its path at (resolved once;
/// `ORD_ABSENT` where the part did not elect it at this kind and scale).
struct ShredFace {
    attno: u32,
    path: Arc<str>,
    kind: ShredLaneKind,
    scale: i32,
    /// D3038: an octet lane's face (`ShredLane::octet`): the column's own sections in every part (`locate`); its values
    /// the part's stored octet-length lane where it sealed one (`stream_of`, `ords`).
    octet: bool,
    ords: Box<[OnceLock<u32>]>,
}

struct LaneFaces {
    stats: Box<[OnceLock<StatsBody>]>,
    /// D4504: the Stats walk body and the bloom / PSMA bodies (H02 b / c) per part, a warm cache (`held::Warm`) as the
    /// dictionaries are: statements hold their own `Arc` (`RelParts::held_faces`); the content trim lets a part's go after
    /// its data (`OpenPart::evict_faces`) and the faces hook empties its cells (`held::let_stale_faces_go`).
    faces: Box<[held::Warm<FaceBodies>]>,
    /// D4410: a warm cache (`held::Warm`): statements hold their own `Arc` of each entry they read (`RelParts::held`, or
    /// their `StmtDicts` cells) and never borrow from here; the part cache's evicted hook empties it.
    dicts: Box<[held::Warm<DictEntry>]>,
    /// sortpath (code-keyed zones): the CodeZones body and the dictionary its codes index, `None` where the part sealed
    /// none (no DICT_CODES stream, or a part before them), read once per generation when a prefix predicate takes them.
    codes: Box<[held::Warm<CodesEntry>]>,
}

/// sortpath (code-keyed zones): a part's CodeZones body. D4482 b: the part cache's from its read (`_held`, dropped before
/// the body frees), as the dictionary beside it is (its images: pgrc_read's holder charges): a list's warm cell keeps
/// both past the statement that read them.
pub(super) struct ZoneBody {
    _held: memledger::Charge,
    body: Box<[u8]>,
}

impl ZoneBody {
    /// `body`, just read on this thread, on the part cache's holder from now to its drop.
    fn new(body: Box<[u8]>) -> ZoneBody {
        let held = memledger::Charge::open(memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache)), memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, body.len() as u64);
        ZoneBody { _held: held, body }
    }
}

/// sortpath: a list's codes cell entry, a part's CodeZones body and the dictionary its codes index (`None` where the part
/// sealed none). D4552 (a): from the moment a list's warm cell keeps it, its own `Arc` block (the handle inline in it) is
/// the part cache's (`warm`), as its body and its dictionary's images are: the trim's evicted hook (`held::let_warm_go`)
/// may drop it last on another backend.
pub(super) struct CodesEntry {
    /// Dropped first, wherever the entry's last holder drops it.
    held: OnceLock<memledger::Charge>,
    zones: Option<(ZoneBody, DictHandle)>,
}

impl CodesEntry {
    fn new(zones: Option<(ZoneBody, DictHandle)>) -> CodesEntry {
        CodesEntry { held: OnceLock::new(), zones }
    }

    /// The entry, its `Arc` block just allocated on this thread, offered to a list's warm cell: the block on the part
    /// cache's holder.
    fn warm(&self) {
        let _ = self.held.set(memledger::Charge::open(FACES_HOLDER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, memledger::arc_bytes::<CodesEntry>()));
    }
}

impl core::ops::Deref for ZoneBody {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.body
    }
}

/// A lane's bloom and PSMA section bodies in one part (unwrapped), `None` where the part sealed none.
struct MetaBodies {
    bloom: Option<Box<[u8]>>,
    psma: Option<Box<[u8]>>,
}

/// D4504: one (face, part)'s metadata bodies (`LaneFaces::faces`): the Stats walk body and the bloom / PSMA bodies, each
/// read once while the entry lives. They are their part's faces in the part cache (`hold`: its census and budget, and
/// its content trim's last tier) unless the metacache switch is off (`inputs::metacache_enabled`: charged only, as D4482
/// a did). `hold` first: its drop moves the bodies' bytes back to the freeing thread before they free.
pub(super) struct FaceBodies {
    hold: Mutex<pgrc_read::FaceHold>,
    body: OnceLock<Option<Box<[u8]>>>,
    meta: OnceLock<MetaBodies>,
}

impl FaceBodies {
    fn new(hold: pgrc_read::FaceHold) -> FaceBodies {
        FaceBodies { hold: Mutex::new(hold), body: OnceLock::new(), meta: OnceLock::new() }
    }

    /// `n` bytes of a body just set on this thread are held.
    fn hold(&self, n: usize) {
        if n > 0 {
            pgsync::lock(&self.hold).add(n as u64);
        }
    }

    /// Whether a content trim let the part's faces go since this entry was made (the faces hook empties its cell).
    pub(super) fn stale(&self) -> bool {
        pgsync::lock(&self.hold).stale()
    }
}

/// D4482: the holder of a list's faces (`Faces::held`): the part cache, whose parts the list keeps open for its
/// generation and whose evictions let its warm cells go.
const FACES_HOLDER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache));

/// The bytes of a body a face keeps.
fn body_bytes(b: &Option<Box<[u8]>>) -> usize {
    b.as_ref().map_or(0, |b| b.len())
}

impl Faces {
    fn new(nparts: usize) -> Faces {
        let held = Mutex::new(memledger::Charge::open(FACES_HOLDER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, 0));
        let lanes = (0..MAX_FACE_ATTNO + MAX_SHRED_FACES).map(|_| OnceLock::new()).collect(); // alloc: Cold
        let f = Faces { held, lanes, shred: (0..MAX_SHRED_FACES).map(|_| OnceLock::new()).collect(), nparts }; // alloc: Cold
        f.hold(core::mem::size_of_val(&*f.lanes) + core::mem::size_of_val(&*f.shred));
        f
    }

    /// D4482: `bytes` more of the faces, just allocated on this thread, are the part cache's (`held`).
    fn hold(&self, bytes: usize) {
        if bytes > 0 {
            pgsync::lock(&self.held).commit(bytes as u64);
        }
    }

    /// Whether `face` is a shred lane's (its records are the path's, never the column's catalog answers or sidecars).
    fn is_shred(face: u32) -> bool {
        face as usize >= MAX_FACE_ATTNO
    }

    /// The face of shred lane `s` over jsonb column `attno`: the slot holding its identity, claimed on first use; `None`
    /// when every slot holds another lane.
    fn shred_face(&self, attno: u32, s: &ShredLane) -> Option<u32> {
        let n = self.nparts;
        // bounded("small-fixed: the list's shred face slots, at most MAX_SHRED_FACES (64)")
        for (i, cell) in self.shred.iter().enumerate() {
            let f = cell.get_or_init(|| ShredFace { attno, path: Arc::clone(&s.path), kind: s.kind, scale: s.scale, octet: s.octet, ords: (0..n).map(|_| OnceLock::new()).collect() }); // alloc: Cold
            if f.attno == attno && *f.path == *s.path && f.kind == s.kind && f.scale == s.scale && f.octet == s.octet {
                return Some((MAX_FACE_ATTNO + i) as u32);
            }
        }
        None
    }

    /// The `(attno, path_ord)` of face `face`'s streams and sections in part `p`: a column's own (`path_ord` 0), or the
    /// shred lane's path as `part` elected it (resolved once per part). `None`: a shred lane `part` did not elect at its
    /// kind and scale, which the part publishes all-invalid (every invalid row walks its image).
    fn locate(&self, face: u32, p: usize, part: &OpenPart) -> PgResult<Option<(u32, u32)>> {
        let Some(i) = (face as usize).checked_sub(MAX_FACE_ATTNO) else { return Ok(Some((face, 0))) };
        let f = self.shred[i].get().ok_or_else(|| err("gravity: an unclaimed shred lane face"))?;
        if f.octet {
            return Ok(Some((f.attno, 0)));
        }
        if let Some(&o) = f.ords[p].get() {
            return Ok((o != ORD_ABSENT).then_some((f.attno, o)));
        }
        let o = match shred_lane_elected(part, f.attno, &f.path, f.kind, f.scale)? {
            Elected::At(o) => o,
            Elected::No | Elected::OtherKind | Elected::OtherScale => ORD_ABSENT,
        };
        let _ = f.ords[p].set(o);
        Ok((o != ORD_ABSENT).then_some((f.attno, o)))
    }

    /// The `(attno, path_ord)` of face `face`'s value stream in part `p`: `locate`'s, except an octet face's (D3038 R1),
    /// whose values are the part's stored octet-length lane where it sealed one (resolved once per part), else the
    /// column's own; its Stats, bloom, PSMA and dictionary stay the column's (`locate`).
    fn stream_of(&self, face: u32, p: usize, part: &OpenPart) -> PgResult<Option<(u32, u32)>> {
        let Some(f) = (face as usize).checked_sub(MAX_FACE_ATTNO).and_then(|i| self.shred[i].get()).filter(|f| f.octet) else {
            return self.locate(face, p, part);
        };
        if let Some(&o) = f.ords[p].get() {
            return Ok(Some((f.attno, o)));
        }
        let o = octet_lane_ord(part, f.attno)?.unwrap_or(0);
        let _ = f.ords[p].set(o);
        Ok(Some((f.attno, o)))
    }

    /// Whether face `face` is a shred lane part `p` resolved as not elected (`locate` ran first: the cursors' open).
    fn absent(&self, face: u32, p: usize) -> bool {
        (face as usize).checked_sub(MAX_FACE_ATTNO).is_some_and(|i| self.shred[i].get().is_some_and(|f| f.ords[p].get() == Some(&ORD_ABSENT)))
    }

    /// FASTADD: the column `face` reads: its attno, or a shred (or octet) lane's jsonb / text column (`None`: an unclaimed
    /// shred face).
    fn attno_of(&self, face: u32) -> Option<u32> {
        match (face as usize).checked_sub(MAX_FACE_ATTNO) {
            None => Some(face),
            Some(i) => self.shred[i].get().map(|f| f.attno),
        }
    }

    fn lane(&self, attno: u32) -> &LaneFaces {
        let n = self.nparts;
        self.lanes[attno as usize].get_or_init(|| {
            let l = LaneFaces { stats: (0..n).map(|_| OnceLock::new()).collect(), faces: (0..n).map(|_| held::Warm::new()).collect(), dicts: (0..n).map(|_| held::Warm::new()).collect(), codes: (0..n).map(|_| held::Warm::new()).collect() }; // alloc: Cold
            self.hold(core::mem::size_of_val(&*l.stats) + core::mem::size_of_val(&*l.faces) + core::mem::size_of_val(&*l.dicts) + core::mem::size_of_val(&*l.codes));
            l
        })
    }
}

/// A shred face's `path_ord` in a part that did not elect its path at its kind and scale (a real `path_ord` is a
/// PathTable position + 1, far below).
const ORD_ABSENT: u32 = u32::MAX;

fn fnv(h: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(h, |h, &b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

impl PartList {
    pub fn new(parts: Vec<PartSpec>) -> PartList {
        let mut starts = Vec::with_capacity(parts.len()); // alloc: Exec
        let (mut granules, mut rows, mut deleted) = (0u64, 0u64, 0u64);
        let mut generation = 0xcbf2_9ce4_8422_2325u64;
        // bounded("catalog-sized: one entry per part of the relation's catalog list, hashed once per list built")
        for p in &parts {
            starts.push(granules);
            granules += p.granules as u64;
            rows += p.rows;
            deleted += p.deletes.as_ref().map_or(0, |d| d.deleted_rows);
            // D1125: the open part's identity, never its path alone. pgrc reuses a part's file name (a same-transaction
            // TRUNCATE restarts at part-0; a rolled-back publish's numbers are taken again), and a new part of the same rows,
            // granules and length then hashed equal: the zone store served the old part's census / order / extrema and a
            // parked source kept its cursors on the unlinked old file (`cursor_generation`, fetch_site's `held_generation`).
            // The uuid is the open instance's (dev, ino, len, footer_off); the section table's crc is the content's, for an
            // inode the file system hands out again.
            generation = match (&p.part, &p.io) {
                (Some(part), _) => fnv(fnv(generation, &part.uuid()), &part.footer().section_table_crc.to_le_bytes()),
                (None, PartIoSpec::Path(c)) => fnv(generation, c.as_bytes()),
                (None, PartIoSpec::Mem { dev, ino, .. }) => fnv(fnv(generation, &dev.to_le_bytes()), &ino.to_le_bytes()),
            };
            generation = fnv(generation, &p.rows.to_le_bytes());
            generation = fnv(generation, &p.granules.to_le_bytes());
            generation = fnv(generation, &p.expect.file_len.unwrap_or(0).to_le_bytes());
        }
        let faces = Faces::new(parts.len());
        // the paths are each part's own block (a `CString`'s bytes and its nul)
        let paths: usize = parts.iter().map(|p| if let PartIoSpec::Path(c) = &p.io { c.as_bytes_with_nul().len() } else { 0 }).sum();
        let own = parts.capacity() * core::mem::size_of::<PartSpec>() + starts.capacity() * core::mem::size_of::<u64>() + paths;
        let held = memledger::Charge::open(FACES_HOLDER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, own as u64);
        PartList { held, parts, starts, granules, rows, deleted, generation, answers: None, faces, zones: OnceLock::new(), lacking: None, derived: Mutex::new(Vec::new()), warm_off: pgsync::atomic::AtomicBool::new(false), registered: pgsync::atomic::AtomicBool::new(false), zones_charge: OnceLock::new() } // alloc: Cold
    }

    /// D4552 (a): the list about to be shared (the residency table's, its statements'): the `Arc` block it goes into joins
    /// its charge (`held`).
    pub fn arc_held(mut self) -> PartList {
        self.held.commit(memledger::arc_bytes::<PartList>());
        self
    }

    /// The relation's generation as the scan resolved it: the identity of every part in order.
    pub fn with_answers(parts: Vec<PartSpec>, answers: Option<Arc<pgrc_am::answers::Answers>>) -> PartList {
        let mut l = PartList::new(parts);
        l.answers = answers;
        l
    }

    /// Part `p`'s part-grain Stats record for `attno` from the catalog's answers (`None`: not held, or a column the part
    /// lacks: its stand-in's Stats answer, FASTADD).
    pub fn answer_record(&self, p: usize, attno: u32) -> Option<&[u8]> {
        if self.lacks(p, attno) {
            return None;
        }
        self.answers.as_ref()?.part_record(attno, p)
    }

    /// `answer_record` of face `face`: a shred lane's never (the catalog holds its column's records, not its path's).
    fn face_record(&self, p: usize, face: u32) -> Option<&[u8]> {
        if Faces::is_shred(face) {
            return None;
        }
        self.answer_record(p, face)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// D2985: the dictionaries the list's shared cells hold (a statement that releases part by part opens none of them).
    /// D4410: its warm cells (`held::Warm`), empty once the part cache's evicted hook let them go.
    pub fn shared_dicts(&self) -> usize {
        self.faces.lanes.iter().filter_map(OnceLock::get).map(|l| l.dicts.iter().filter(|c| c.is_set()).count()).sum()
    }

    /// The list's zone columns, set once: `resolve`'s from the relation footer, or an in-memory list's (tests, tools) as
    /// the footer's would be; a list that has its columns keeps them.
    pub fn set_zone_columns(&self, cols: Vec<pgrc_am::footer::ZoneFacts>) {
        self.set_zones(cols, true);
    }

    /// `own`: no generation's facts hold the columns, so the list's charge counts them too.
    fn set_zones(&self, cols: Vec<pgrc_am::footer::ZoneFacts>, own: bool) {
        let mut held = cols.len() * core::mem::size_of::<(u32, Arc<ZoneColumn>)>();
        if own {
            held += cols.iter().map(pgrc_am::footer::ZoneFacts::heap_bytes).sum::<usize>();
        }
        if self.zones.set(cols.into_iter().map(|z| (z.attno, z.col)).collect()).is_ok() { // alloc: Cold charged the part cache (zones_charge)
            const OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache));
            let _ = self.zones_charge.set(memledger::Charge::open(OWNER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, held as u64));
        }
    }

    pub fn parts(&self) -> &[PartSpec] {
        &self.parts
    }

    pub fn starts(&self) -> &[u64] {
        &self.starts
    }

    pub fn granules(&self) -> u64 {
        self.granules
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// The parts' deleted rows (their delete vectors'), summed once as the list is built.
    pub fn deleted(&self) -> u64 {
        self.deleted
    }

    pub fn len(&self) -> usize {
        self.parts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// The largest part in granules (the upper bound of a startup claim).
    pub fn max_part_granules(&self) -> u64 {
        self.parts.iter().map(|p| p.granules as u64).max().unwrap_or(0)
    }

    pub fn part_of(&self, g: u64) -> usize {
        self.starts.partition_point(|&s| s <= g).saturating_sub(1)
    }

    pub fn end_of(&self, p: usize) -> u64 {
        self.starts.get(p + 1).copied().unwrap_or(self.granules)
    }
}

/// The statement's granule relations as the leader resolved them (§4.2), handed to every source through `BindingParts.resolved`.
pub struct Resolved {
    rels: Vec<RelParts>, // alloc: Exec
    tables: StmtTables,
    /// D3206: the fingerprint cache's tables (`Fp128`, entry runs, renders, condition tables) one source built or loaded
    /// for the statement's others on a process-cache miss, capped like `tables` (text_hosts `FpMemo`).
    fp_memo: crate::dict::text_hosts::FpMemo,
    condition_cache_mode: guc_tables::ConditionCacheMode,
    /// D3115: the `PROGRAM` tables' sweeps the statement's sources share.
    sweeps: StmtSweeps,
    /// This execution's verdict cells and bitmap recheck quals by registry index (`cells_from`; empty: each source reads
    /// its recipe's). The qual: a parked Node's bitmap source was derived with the qual of the statement that built it,
    /// compiled in that execution's context and dropped at its unbind; each bind takes this execution's from here
    /// (`BitmapSource::bind`), as a granule source takes its verdict cell.
    cells: Vec<(Option<Arc<VerdictCell>>, Option<crate::ops::scan::raw::LeaderFilter>)>, // alloc: Exec
    /// V4R-310: the statement runs under a transaction snapshot (RR / SERIALIZABLE), read on the leader (`entries_safe`).
    xact_snapshot: bool,
    /// The walk orders by registry site (`usize::MAX`: a source at no site), never by relation: an InitPlan's zone walk and
    /// its parent's scan of the same relation are two sites, and the parent's plain claims erased the walk's order (D2897).
    orders: Mutex<Vec<(usize, Arc<[u32]>)>>, // alloc: Exec
    /// D2985: the statement streams its one granule site's rows into its sink (`set_part_release`), and the part-cache
    /// level under which its dictionaries go to the list's cells (`None`: the process budget).
    part_release: bool,
    /// D3268: the statement's one granule site feeds an aggregate: its parts' uses (not its dictionaries) are released as
    /// its claims leave them (`set_part_uses_release`).
    part_uses: bool,
    /// D3823: and its dictionaries past the part cache's level go with each part (`set_part_own_dicts`).
    part_own_dicts: bool,
    /// D3823 (R3b): a program over several sites: each relation whose every site is read once by claim, with its readers
    /// (the sites over it), releases its parts' uses once every reader's claims left each part (`set_part_uses_rels`).
    uses_rels: Box<[(Oid, u32)]>,
    /// D3823: the listed relations whose dictionaries past the part cache's level go with each part (`set_part_own_dicts_rels`).
    own_dicts_rels: Box<[Oid]>,
    part_share: Option<u64>,
    /// D3823 gate: the part-cache level past which a uses-only release gives its part back (`None`: the cache's budget).
    part_release_at: Option<u64>,
    /// D3117: this execution's frozen index images of the sites a worker probes, by registry index (`file_probe`; exec
    /// runner plan.rs captures them), which each worker's probe filter binds (index_probe.rs `ProbeFilter::bind`).
    probes: Vec<Option<Arc<crate::ops::scan::index_probe::Filed>>>, // alloc: Exec
    /// D3156: each site's kept-mode code tables of its verbatim parts, by registry index (kept.rs; made with `cells`).
    kept: Vec<OnceLock<Box<kept::SiteKept>>>, // alloc: Exec
    /// This execution's leaf walk slots of the index-only sites the pool reads, by registry index (`file_leaf_cell`; exec
    /// runner plan.rs fills each when its pipeline's run starts), which each participant's leaf source binds
    /// (index_leaves.rs `LeafSource::bind`).
    leaf_cells: Vec<Option<Arc<crate::ops::scan::index_leaves::LeafCell>>>, // alloc: Exec
    /// This execution's ranges slots of the ordered sites the pool reads in key ranges, by registry index
    /// (`file_range_cell`; exec runner plan.rs fills each when its pipeline's run starts), which each participant's range
    /// source binds (index_ranges.rs `RangeSource::bind`).
    range_cells: Vec<Option<Arc<crate::ops::scan::index_ranges::RangeCell>>>, // alloc: Exec
    /// D3585: this execution's relations of each relation-set site, by registry index (`file_rel_set`; exec runner plan.rs
    /// `claims_of` resolves them where it cuts the site's claims), which each participant's heap source binds.
    rel_sets: Vec<Option<Arc<crate::ops::scan::heap_source::RelSet>>>, // alloc: Exec
    /// D3586: the sites whose every driving pipeline is an Append arm this execution's initial pruning removed, a bit per
    /// registry index (`prune_site`; exec runner plan.rs files them before the sources bind): a heap range source there
    /// binds as a relation of no blocks, its relation never opened (heap_source.rs `bind_pruned`).
    pruned: Vec<u64>, // alloc: Exec
    /// Memory step 6a: this execution's part re-use token (`partcache::next_stmt_token`): its uses of a part, from any
    /// worker, are one statement's.
    token: u64,
}

/// The statement's shared tables (partitioned-record.md R4: per statement, never across): a
/// `TableSpec::shared` table is built once per (relation, part, column, spec) by the first source
/// that needs it and read by every other — the two-pass form's pass 2 finds pass 1's — LRU by
/// bytes under `cap` (the grant; a source holding an evicted table keeps it), gone with the `Resolved`.
pub struct StmtTables {
    map: Mutex<HashMap<StmtKey, StmtEntry>>,
    bytes: AtomicU64,
    cap: AtomicU64,
    tick: AtomicU64,
}

/// A shared table's identity in the statement: the relation's index in the `Resolved`, the part, the column, the spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StmtKey {
    pub rel: u32,
    pub part: u32,
    pub attno: u32,
    pub fingerprint: u64,
}

struct StmtEntry {
    slot: Arc<Mutex<Option<Arc<crate::dict::text_hosts::FpTable>>>>,
    bytes: u64,
    used: u64,
}

impl StmtTables {
    fn new() -> StmtTables {
        StmtTables { map: Mutex::new(HashMap::new()), bytes: AtomicU64::new(0), cap: AtomicU64::new(0), tick: AtomicU64::new(0) } // alloc: Exec
    }

    /// The table at `key`: built by `build` under the key's own lock when no source built it yet.
    pub fn get_or_build(&self, key: StmtKey, build: &mut dyn FnMut() -> PgResult<crate::dict::text_hosts::FpTable>) -> PgResult<Arc<crate::dict::text_hosts::FpTable>> {
        let mut step = cancelpoll::step("granule_source StmtTables::get_or_build");
        let slot = {
            let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
            let tick = self.tick.fetch_add(1, Ordering::Relaxed);
            let e = m.entry(key).or_insert_with(|| StmtEntry { slot: Arc::new(Mutex::new(None)), bytes: 0, used: tick }); // alloc: Exec
            e.used = tick;
            Arc::clone(&e.slot)
        };
        let mut g = slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = &*g {
            TABLES_SHARED.fetch_add(1, Ordering::Relaxed);
            return Ok(Arc::clone(t));
        }
        let t = Arc::new(build()?); // alloc: Exec
        *g = Some(Arc::clone(&t));
        drop(g);
        let bytes = t.bytes();
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = m.get_mut(&key) {
            e.bytes = bytes;
            self.bytes.fetch_add(bytes, Ordering::Relaxed);
        }
        let cap = self.cap.load(Ordering::Relaxed);
        // a tick per table evicted; a cancel leaves the rest over the cap to the next build's check
        while self.bytes.load(Ordering::Relaxed) > cap {
            let Some(oldest) = m.iter().filter(|(_, e)| e.bytes > 0).min_by_key(|(_, e)| e.used).map(|(k, _)| *k) else { break };
            let e = m.remove(&oldest).expect("present");
            self.bytes.fetch_sub(e.bytes, Ordering::Relaxed);
            if step.tick().is_err() {
                break;
            }
        }
        Ok(t)
    }

    /// The census: `(tables held, their bytes)`.
    pub fn held(&self) -> (u64, u64) {
        let m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        (m.values().filter(|e| e.bytes > 0).count() as u64, self.bytes.load(Ordering::Relaxed))
    }
}

impl Default for Resolved {
    fn default() -> Resolved {
        Resolved::new()
    }
}

impl Resolved {
    pub fn new() -> Resolved {
        let r = Resolved { rels: Vec::new(), tables: StmtTables::new(), fp_memo: crate::dict::text_hosts::FpMemo::new(), condition_cache_mode: crate::inputs::condition_cache_mode(), sweeps: StmtSweeps::new(), cells: Vec::new(), xact_snapshot: crate::xact_snapshot(), orders: Mutex::new(Vec::new()), part_release: false, part_uses: false, part_own_dicts: false, uses_rels: Box::default(), own_dicts_rels: Box::default(), part_share: None, part_release_at: None, probes: Vec::new(), kept: Vec::new(), leaf_cells: Vec::new(), range_cells: Vec::new(), rel_sets: Vec::new(), pruned: Vec::new(), token: pgrc_read::partcache::next_stmt_token() }; // alloc: Exec
        r.fp_memo.set_cross_statement(r.condition_cache_mode >= guc_tables::ConditionCacheMode::Rows && crate::inputs::dict_filter_cache_on());
        r
    }

    /// D300: the verdict cells of `registry` (the executing statement's), by registry index. A parked Node's sources
    /// were derived from the registry of the statement that built the Node, whose recipe clones hold THAT admission's
    /// cell; each binds this execution's cell from here (`GranuleRangeSource::cell`), the one src.verdicts wrote. A bitmap
    /// entry's recheck qual beside it (`recheck`), in the same vector (one allocation per execution).
    pub fn cells_from(&mut self, registry: &crate::ops::HostRegistry) {
        self.cells.clear();
        self.cells.extend((0..registry.len()).map(|i| (registry.granule_recipe(i).map(|r| Arc::clone(&r.verdicts)), registry.bitmap_recheck(i).cloned().map(crate::ops::scan::raw::LeaderFilter::new)))); // alloc: Exec
        self.kept.clear();
        self.kept.resize_with(registry.len(), OnceLock::new); // alloc: Exec
    }

    /// The per-site own cells (kept.rs: kept and lens modes) of `sites` sites, as `cells_from` sizes them from a registry
    /// (a statement built without one: the host tests' sources `at_site`).
    pub fn site_cells(&mut self, sites: usize) {
        self.kept = (0..sites).map(|_| Default::default()).collect(); // alloc: Exec
    }

    /// D3117: files this execution's image of registry entry `site`'s index for its workers' probes.
    pub fn file_probe(&mut self, site: usize, img: Arc<crate::ops::scan::index_probe::Filed>) {
        if self.probes.len() <= site {
            self.probes.resize(site + 1, None); // alloc: Exec
        }
        self.probes[site] = Some(img);
    }

    /// Registry entry `site`'s bitmap recheck qual for this execution (`cells_from`), for every binding: the leader's and a
    /// worker slot's, whose binds run on the leader's thread (`LeaderFilter`). pglfast#3520: a worker's binding that got
    /// none left a parked worker set's source (its unbind dropped the qual it was derived with) without one at W8.
    pub fn recheck(&self, site: usize) -> Option<crate::FilterFn> {
        self.cells.get(site)?.1.as_ref().map(crate::ops::scan::raw::LeaderFilter::get)
    }

    /// D3117: this execution's image of registry entry `site`'s index (`file_probe`).
    pub fn probe_image(&self, site: usize) -> Option<&Arc<crate::ops::scan::index_probe::Filed>> {
        self.probes.get(site).and_then(Option::as_ref)
    }

    /// Files this execution's leaf walk slot of registry entry `site` (an index-only site the pool reads).
    pub fn file_leaf_cell(&mut self, site: usize, cell: Arc<crate::ops::scan::index_leaves::LeafCell>) {
        if self.leaf_cells.len() <= site {
            self.leaf_cells.resize(site + 1, None); // alloc: Exec
        }
        self.leaf_cells[site] = Some(cell);
    }

    /// This execution's leaf walk slot of registry entry `site` (`file_leaf_cell`).
    pub fn leaf_cell(&self, site: usize) -> Option<&Arc<crate::ops::scan::index_leaves::LeafCell>> {
        self.leaf_cells.get(site).and_then(Option::as_ref)
    }

    /// Files this execution's ranges slot of registry entry `site` (an ordered site the pool reads in key ranges).
    pub fn file_range_cell(&mut self, site: usize, cell: Arc<crate::ops::scan::index_ranges::RangeCell>) {
        if self.range_cells.len() <= site {
            self.range_cells.resize(site + 1, None); // alloc: Exec
        }
        self.range_cells[site] = Some(cell);
    }

    /// D3586: registry entry `site` is read by no pipeline this execution runs: its every driving pipeline is an Append
    /// arm the initial pruning removed (`pruned`).
    pub fn prune_site(&mut self, site: usize) {
        if self.pruned.len() <= site / 64 {
            self.pruned.resize(site / 64 + 1, 0); // alloc: Exec
        }
        self.pruned[site / 64] |= 1 << (site % 64);
    }

    /// Whether registry entry `site` was filed by `prune_site` for this execution.
    pub fn site_pruned(&self, site: usize) -> bool {
        self.pruned.get(site / 64).is_some_and(|w| w >> (site % 64) & 1 != 0)
    }

    /// This execution's ranges slot of registry entry `site` (`file_range_cell`).
    pub fn range_cell(&self, site: usize) -> Option<&Arc<crate::ops::scan::index_ranges::RangeCell>> {
        self.range_cells.get(site).and_then(Option::as_ref)
    }

    /// D3585: files this execution's relations of relation-set site `site` (a registry index).
    pub fn file_rel_set(&mut self, site: usize, set: Arc<crate::ops::scan::heap_source::RelSet>) {
        if self.rel_sets.len() <= site {
            self.rel_sets.resize(site + 1, None); // alloc: Exec
        }
        self.rel_sets[site] = Some(set);
    }

    /// D3585: this execution's relations of relation-set site `site` (`file_rel_set`).
    pub fn rel_set(&self, site: usize) -> Option<&Arc<crate::ops::scan::heap_source::RelSet>> {
        self.rel_sets.get(site).and_then(Option::as_ref)
    }

    /// The verdict cell of registry entry `site` for this execution (`cells_from`).
    fn cell(&self, site: usize) -> Option<&Arc<VerdictCell>> {
        self.cells.get(site)?.0.as_ref()
    }

    /// The shared tables' cap in bytes (the statement's grant); 0 shares nothing. D3206: the fingerprint memo's too.
    pub fn set_table_cap(&self, bytes: u64) {
        self.tables.cap.store(bytes, Ordering::Relaxed);
        self.fp_memo.set_cap(bytes);
    }

    pub fn tables(&self) -> &StmtTables {
        &self.tables
    }

    /// D3206: the statement's fingerprint memo.
    pub fn fp_memo(&self) -> &crate::dict::text_hosts::FpMemo {
        &self.fp_memo
    }

    /// Whether this execution may share predicate tables across statements.
    pub fn cross_statement(&self) -> bool {
        self.fp_memo.cross_statement()
    }

    /// D2985: the statement's only pipeline drives its only (granule) site straight into the statement's sink, which takes
    /// each row before the next window: its relations' dictionaries are the statement's own and released part by part
    /// (`release.rs`). Set before `install`.
    pub fn set_part_release(&mut self, on: bool) {
        self.part_release = on && crate::inputs::part_release_enabled();
    }

    /// D3268 / D3823: the statement's only granule site feeds a breaker (any kind) whose later pipelines read no site and
    /// that no rerun set runs again: each part's use is released once every claim left it, its dictionaries staying in the
    /// list's cells (`release.rs`), so the part cache trims the parts the scan is past under its budget instead of the
    /// statement holding all of them to its end. Set before `install`; `set_part_release` wins.
    pub fn set_part_uses_release(&mut self, on: bool) {
        self.part_uses = on && crate::inputs::part_release_enabled();
    }

    /// D3823: a uses-only release (`set_part_uses_release`) whose breaker keeps no dictionary image by reference past its
    /// window (`mempolicy::release::dicts_go_with_part`) also opens its dictionaries in its own cells past the part
    /// cache's level and drops each with its part's release; under the level they go to the list's cells as before. A
    /// code word the breaker kept renders after the release through the dictionary opened again (`dict_value_pg`).
    /// Set before `install`; `PGRUST_GRAVITY_RELEASE_DICTS=0` keeps them in the list's cells.
    pub fn set_part_own_dicts(&mut self, on: bool) {
        self.part_own_dicts = on && crate::inputs::release_dicts_enabled();
    }

    /// D3823 (R3b): a program over several granule sites: each listed relation (every site over it read once by claim:
    /// one pipeline drives it, no rerun set runs that pipeline again, no two-pass admit) releases its parts' uses as the
    /// uses-only release does, once all of its `readers` sites' claims left each part; its dictionaries stay in the list's
    /// cells. Set before `install`; `set_part_release` / `set_part_uses_release` win.
    pub fn set_part_uses_rels(&mut self, rels: Box<[(Oid, u32)]>) {
        self.uses_rels = if crate::inputs::part_release_enabled() { rels } else { Box::default() };
    }

    /// D3823: of the relations `set_part_uses_rels` lists, those whose every site feeds a breaker that keeps code words, no
    /// dictionary image (`mempolicy::release::dicts_go_with_part`): their dictionaries past the part cache's level go with
    /// each part, as `set_part_own_dicts` does for one site. Set before `install`.
    pub fn set_part_own_dicts_rels(&mut self, rels: Box<[Oid]>) {
        self.own_dicts_rels = if crate::inputs::part_release_enabled() && crate::inputs::release_dicts_enabled() { rels } else { Box::default() };
    }

    /// D3823 gate: the part-cache level past which a uses-only release (D3268, D3823) gives a part back now; under it the
    /// part stays held, its cursors open for the statement's next run, until a later claim finds the cache past the level
    /// (`mempolicy::release`). `None`: the cache's budget, read at each check; a portal and tests: `Some(0)`
    /// always; tests: `Some(u64::MAX)` never. Applies to the relations installed already too.
    pub fn set_part_release_at(&mut self, level: Option<u64>) {
        self.part_release_at = level;
        // bounded("other: the statement's pgrc relations, one level store each: its range table bounds them, as PG's ExecInitNode walks the same range table with no poll")
        for rp in &self.rels {
            if let Some(sd) = rp.stmt.as_deref() {
                sd.set_release_at(level);
            }
        }
    }

    /// The part-cache level under which a releasing statement's dictionaries go to the list's cells (`None`: the process
    /// part cache's budget; tests: `Some(0)`, always the statement's own). Set before `install`.
    pub fn set_part_share(&mut self, below: Option<u64>) {
        self.part_share = below;
    }

    /// Dictionaries the statement opened in its own cells (past the part cache's budget), over every relation.
    pub fn part_release_private(&self) -> u64 {
        self.rels.iter().filter_map(|r| r.stmt.as_deref()).map(release::StmtDicts::private_opens).sum()
    }

    /// `(parts released, dictionaries re-opened)` in this statement, over every relation.
    pub fn part_release_counts(&self) -> (u64, u64) {
        self.rels.iter().filter_map(|r| r.stmt.as_deref()).map(release::StmtDicts::counts).fold((0, 0), |a, c| (a.0 + c.0, a.1 + c.1))
    }

    /// Install `rel`'s list; a second install of a relation replaces the first.
    pub fn install(&mut self, rel: Oid, list: Arc<PartList>) {
        let n = list.len();
        let readers = if self.part_release || self.part_uses { Some(1) } else { self.uses_rels.iter().find(|r| r.0 == rel).map(|r| r.1.max(1)) };
        let own = (self.part_uses && self.part_own_dicts) || (!self.part_uses && self.uses_rels.iter().any(|r| r.0 == rel) && self.own_dicts_rels.contains(&rel));
        let stmt = readers.map(|readers| Box::new(release::StmtDicts::new(&list, self.part_share, self.token, self.part_release, own, readers, self.part_release_at))); // alloc: Exec
        held::register(&list);
        let rp = RelParts {
            rel,
            token: self.token,
            list,
            parts: StmtSlice::collect((0..n).map(|_| OnceLock::new())), // alloc: Exec charged gravity_driver::stmt_context()
            uses: StmtSlice::collect((0..n).map(|_| OnceLock::new())), // alloc: Exec charged gravity_driver::stmt_context()
            opened: AtomicU64::new(0),
            stmt,
            held: held::Held::new(n),
            held_codes: held::Held::new(n),
            held_faces: held::Held::new(n),
        };
        match self.rels.iter().position(|r| r.rel == rel) {
            Some(i) => self.rels[i] = rp,
            None => self.rels.push(rp),
        }
    }

    /// Site `site`'s walk order over its relation's granules (`None`: its claims' units are granules).
    pub fn set_order(&self, site: usize, order: Option<Arc<[u32]>>) {
        let mut m = self.orders.lock().unwrap_or_else(|e| e.into_inner());
        m.retain(|(s, _)| *s != site);
        if let Some(o) = order {
            m.push((site, o));
        }
    }

    pub fn order_of(&self, site: usize) -> Option<Arc<[u32]>> {
        self.orders.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(s, _)| *s == site).map(|(_, o)| Arc::clone(o))
    }

    /// A statement over one recipe's relation (the tests, the tools).
    pub fn for_recipe(recipe: &GranuleRecipe, list: Arc<PartList>) -> Resolved {
        Resolved::for_recipe_ordered(recipe, list, None)
    }

    /// A statement over one relation's list (a fetch site alone).
    pub fn for_rel(rel: Oid, list: Arc<PartList>) -> Resolved {
        let mut r = Resolved::new();
        r.install(rel, list);
        r
    }

    /// `for_recipe` with the zone walk's order over the granules, for a source at no site.
    pub fn for_recipe_ordered(recipe: &GranuleRecipe, list: Arc<PartList>, order: Option<Arc<[u32]>>) -> Resolved {
        let mut r = Resolved::new();
        r.install(recipe.rel, list);
        r.set_order(usize::MAX, order);
        r
    }

    pub fn rel(&self, rel: Oid) -> Option<&RelParts> {
        self.rels.iter().find(|r| r.rel == rel)
    }

    pub fn list(&self, rel: Oid) -> Option<&Arc<PartList>> {
        self.rel(rel).map(|r| &r.list)
    }

    fn index_of(&self, rel: Oid) -> Option<usize> {
        self.rels.iter().position(|r| r.rel == rel)
    }

    /// The statement's `Resolved` behind a binding.
    pub fn of<'b>(b: &BindingRef<'b>) -> PgResult<&'b Resolved> {
        b.resolved.and_then(|r| r.downcast_ref::<Resolved>()).ok_or_else(|| err("gravity: granule source bound without a resolved relation"))
    }
}

/// CONVERT wave 2 (R1 (ii), R2): a per-part table the statement's sources share (no breaker owns it), with the door-1
/// charge of its bytes on the statement or the portal (`gravity_driver::stmt_context`, `Kind::Region`): opened on the
/// allocating thread right after the allocation, and declared first, so its bytes reach the dropping thread's account
/// before the free. Uncharged with no statement context (tests, tools). It reads as its slice.
pub struct StmtSlice<T> {
    _charge: memledger::Charge,
    cells: Box<[T]>,
}

impl<T> StmtSlice<T> {
    pub(super) fn collect(it: impl Iterator<Item = T>) -> StmtSlice<T> {
        let cells: Box<[T]> = it.collect(); // alloc: Exec charged gravity_driver::stmt_context()
        let bytes = core::mem::size_of_val(&*cells) as u64;
        let _charge = gravity_driver::stmt_context().map_or(memledger::Charge::NONE, |(o, r)| memledger::Charge::open(o, r, memledger::Kind::Region, memledger::charge::Door::Heap, bytes));
        StmtSlice { _charge, cells }
    }
}

impl<T> core::ops::Deref for StmtSlice<T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        &self.cells
    }
}

/// One relation of the statement, shared read-only by every source over it: the list (whose
/// `Faces` hold the per-(attno, part) Stats faces and dictionaries for the generation) and the
/// parts opened for the statement, filled once by the first thread that needs them.
pub struct RelParts {
    rel: Oid,
    /// The statement's part re-use token (`Resolved::token`).
    token: u64,
    list: Arc<PartList>,
    parts: StmtSlice<OnceLock<Arc<OpenPart>>>,
    /// D3003: the statement's use of each part it opened: its cached contents are not evicted while the statement runs.
    uses: StmtSlice<OnceLock<pgrc_read::PartUse>>,
    opened: AtomicU64,
    /// D2985: a streaming statement's own dictionary cells, released part by part (`release.rs`); `None`: the list's.
    stmt: Option<Box<release::StmtDicts>>,
    /// D4410: the statement's own `Arc` of each dictionary it read that it keeps to its end (`held.rs`; those released
    /// with their part are in `stmt`'s cells), and of each code-zones body + dictionary: readers borrow from here, never
    /// from the list's warm cells.
    held: held::Held<DictEntry>,
    held_codes: held::Held<CodesEntry>,
    /// D4504: the statement's own `Arc` of each (face, part)'s metadata bodies it read (`face_bodies`): readers borrow from
    /// here, never from the list's warm cells, which the content trim may empty under a running statement.
    held_faces: held::Held<FaceBodies>,
}

impl RelParts {
    pub fn list(&self) -> &Arc<PartList> {
        &self.list
    }

    /// Registry opens this relation's parts cost the statement (a resident part costs none).
    pub fn parts_opened(&self) -> u64 {
        self.opened.load(Ordering::Relaxed)
    }

    /// The live uses of part `p` (every statement's and every cursor's; 0 for a part this statement never opened): the
    /// release's witness (D3823).
    pub fn part_users(&self, p: usize) -> u32 {
        self.parts.get(p).and_then(OnceLock::get).map_or(0, |part| part.users())
    }

    /// The parts open for the statement and their resident bytes.
    pub fn residency(&self) -> Residency {
        let held = self.parts.iter().filter_map(OnceLock::get);
        Residency { parts: held.clone().count() as u32, bytes: held.map(|p| p.resident()).sum() }
    }

    /// Part `p` open: the resident table's handle, else opened through `registry` once for the statement.
    fn part(&self, p: usize, registry: &PartRegistry) -> PgResult<&Arc<OpenPart>> {
        if let Some(part) = self.parts[p].get() {
            if let Some(sd) = self.stmt.as_deref() {
                sd.hold(p, part);
            }
            return Ok(part);
        }
        // D3063 (2a): a statement past its memory limit or the governor's bar stops at the next part it opens (its part
        // cache faults count there, and a scan may allocate no memory-context block where the governor would see it)
        if let Some(e) = mcx::governor::refusal("pgrc part cache") {
            return Err(Box::<PgError>::new(e)); // alloc: Cold
        }
        let spec = &self.list.parts()[p];
        let part = match &spec.part {
            Some(part) => Arc::clone(part),
            None => {
                self.opened.fetch_add(1, Ordering::Relaxed);
                Arc::clone(registry.open_pinned(&spec.expect, || open_io(&spec.io)).map_err(read_err)?.part())
            }
        };
        if part.footer().granule_count != spec.granules || part.grain().rows() as usize > ROWS {
            return Err(err("gravity: pgrc part geometry changed under the plan"));
        }
        match self.stmt.as_deref() {
            Some(sd) => sd.hold(p, &part),
            None => {
                let _ = self.uses[p].set(part.use_part_in(self.token));
            }
        }
        let _ = self.parts[p].set(part);
        // D3003: a statement's first touch of a part is where the process part cache answers its budget (one relaxed
        // load pair under it): the least recently used parts no statement uses give their contents back
        let _trim = cancelpoll::step("granule_source RelParts::part: the part cache's trim");
        registry.trim_if_over();
        Ok(self.parts[p].get().expect("set"))
    }

    fn part_if_open(&self, p: usize) -> Option<&Arc<OpenPart>> {
        self.parts[p].get()
    }

    /// FASTADD: the part face `face` of part `p` reads, `part` (part `p`, open) or the stand-in for a column the part
    /// lacks (`granule_source/fastadd.rs`): its streams, sections and dictionary are the face's.
    fn lane<'a>(&'a self, p: usize, face: u32, part: &'a Arc<OpenPart>) -> PgResult<&'a Arc<OpenPart>> {
        match (self.list.lacking.is_some(), self.list.faces.attno_of(face)) {
            (true, Some(attno)) => self.list.lane_part(p, attno, part),
            _ => Ok(part),
        }
    }

    /// The part-grain Stats face of `face` in part `p`: the catalog's record, else the body read from the part (`NoAnswers`).
    fn stats(&self, p: usize, face: u32, part: &Arc<OpenPart>) -> PgResult<&StatsBody> {
        let cell = &self.list.faces.lane(face).stats[p];
        if let Some(s) = cell.get() {
            return Ok(s);
        }
        let part = self.lane(p, face, part)?;
        let s = if self.list.face_record(p, face).is_some() {
            ANSWERS_SERVED.fetch_add(1, Ordering::Relaxed);
            StatsBody::Catalog
        } else {
            // sortpath (2): a shred face's records are its path's, which the catalog never holds (`face_record`); they are
            // read for the path's zone walk (lane zones), the declared granule-walk reason
            let why = if Faces::is_shred(face) {
                FirstTouch::GranuleWalk
            } else if self.list.faces.attno_of(face).is_some_and(|a| self.list.lacks(p, a)) {
                FirstTouch::StandIn
            } else {
                FirstTouch::NoAnswers
            };
            match self.list.faces.locate(face, p, part)? {
                Some((attno, ord)) => stats_body(part, attno, ord, why)?.map_or(StatsBody::None, StatsBody::Owned),
                None => StatsBody::None,
            }
        };
        let n = if let StatsBody::Owned(b) = &s { b.len() } else { 0 };
        if cell.set(s).is_ok() {
            self.list.faces.hold(n);
        }
        Ok(cell.get().expect("set"))
    }

    /// D4504: the statement's own entry of `face`'s metadata bodies in part `p`: the one it holds, else the list's warm one,
    /// else a new one (kept warm for the next statement where the list keeps them), held to the statement's end.
    fn face_bodies(&self, p: usize, face: u32, part: &OpenPart) -> &FaceBodies {
        if let Some(f) = self.held_faces.get(p, face) {
            return f;
        }
        let cell = &self.list.faces.lane(face).faces[p];
        // a stale entry (the content trim let its part's faces go before the faces hook emptied the cell) is replaced
        let f = match cell.get().filter(|f| !f.stale()) {
            Some(f) => f,
            None => {
                // the part's `Arc` the statement opened (the list's otherwise): its faces are counted as that part's
                let arc = self.part_if_open(p).or(self.list.parts()[p].part.as_ref()).filter(|a| core::ptr::eq(a.as_ref(), part));
                let hold = arc.map_or_else(pgrc_read::FaceHold::unpinned, |a| pgrc_read::FaceHold::new(a, crate::inputs::metacache_enabled()));
                let f = Arc::new(FaceBodies::new(hold)); // alloc: Exec charged FaceHold
                // its own block is its part's face too (an `Arc`'s two counts beside it)
                f.hold(core::mem::size_of::<FaceBodies>() + 2 * core::mem::size_of::<usize>());
                // the list keeps every entry, as its cells did before (the part cache's trim bounds them, metacache on)
                cell.put_live(f, FaceBodies::stale)
            }
        };
        self.held_faces.publish(p, face, f)
    }

    /// D4504: the entry of `face`'s bodies in part `p` the statement holds, else the list's warm one (adopted), else none.
    fn face_bodies_if_read(&self, p: usize, face: u32) -> Option<&FaceBodies> {
        if let Some(f) = self.held_faces.get(p, face) {
            return Some(f);
        }
        let f = self.list.faces.lane(face).faces[p].get()?;
        Some(self.held_faces.publish(p, face, f))
    }

    /// The whole Stats body of `face` in part `p` for a granule walk: read from the part once (`GranuleWalk`) unless the face holds it.
    fn body(&self, p: usize, face: u32, part: &Arc<OpenPart>) -> PgResult<Option<&[u8]>> {
        let lane = self.list.faces.lane(face);
        if let Some(StatsBody::Owned(b)) = lane.stats[p].get() {
            return Ok(Some(b));
        }
        let f = self.face_bodies(p, face, part);
        if f.body.get().is_none() {
            let part = self.lane(p, face, part)?;
            let b = match self.list.faces.locate(face, p, part)? {
                Some((attno, ord)) => stats_body(part, attno, ord, FirstTouch::GranuleWalk)?,
                None => None,
            };
            let n = body_bytes(&b);
            if f.body.set(b).is_ok() {
                f.hold(n);
            }
        }
        Ok(f.body.get().expect("set").as_deref())
    }

    /// The bloom and PSMA bodies of `face` in part `p`, read from the part once while the list's entry lives.
    fn meta(&self, p: usize, face: u32, part: &Arc<OpenPart>) -> PgResult<&MetaBodies> {
        let f = self.face_bodies(p, face, part);
        if let Some(m) = f.meta.get() {
            return Ok(m);
        }
        let part = self.lane(p, face, part)?;
        let m = match self.list.faces.locate(face, p, part)? {
            Some((attno, ord)) => MetaBodies { bloom: section_body(part, SectionKind::Bloom, attno, ord)?, psma: section_body(part, SectionKind::Psma, attno, ord)? },
            None => MetaBodies { bloom: None, psma: None },
        };
        let n = body_bytes(&m.bloom) + body_bytes(&m.psma);
        if f.meta.set(m).is_ok() {
            f.hold(n);
        }
        Ok(f.meta.get().expect("set"))
    }

    /// Part `p`'s meta sections of `lanes` the faces do not hold yet (the Stats body; bloom and PSMA for a lane marked), faulted
    /// in one positioned read per cluster (`section_bytes_batch`: a column's meta sections sit together after its streams),
    /// so the per-section reads `stats` / `body` / `meta` make next hit the segment cache: otherwise a cold scan reads 3 small
    /// sections per part one pread each. A held face adds nothing, and fewer than two sections read as before. The run's read
    /// buffer is `owner`'s (the reading source's held owner: cvpread's `section_bytes_batch_for`).
    fn prefault_meta(&self, p: usize, lanes: &[(u32, bool)], part: &OpenPart, owner: HeldOwner) -> PgResult<()> {
        let mut idxs = Vec::with_capacity(3 * lanes.len()); // alloc: Cold
        // bounded("key-columns: one face per asked lane, at most MaxTupleAttributeNumber (1664) columns; the reads are one batch after the loop")
        for &(face, meta) in lanes {
            // FASTADD: a stand-in's sections are in memory, never in `part`
            if self.list.faces.attno_of(face).is_some_and(|a| self.list.lacks(p, a)) {
                continue;
            }
            let lane = self.list.faces.lane(face);
            let f = self.face_bodies_if_read(p, face);
            let (body_read, meta_read) = (f.is_some_and(|f| f.body.get().is_some()), f.is_some_and(|f| f.meta.get().is_some()));
            let body_held = body_read || matches!(lane.stats[p].get(), Some(StatsBody::Owned(_)));
            let want = [(SectionKind::Stats, !body_held), (SectionKind::Bloom, meta && !meta_read), (SectionKind::Psma, meta && !meta_read)];
            let Some((attno, ord)) = self.list.faces.locate(face, p, part)? else { continue };
            // bounded("small-fixed: the three meta sections of a face: Stats, Bloom and PSMA")
            for (kind, on) in want {
                if let Some(idx) = on.then(|| part.find_section(kind, attno, ord)).flatten() {
                    if !idxs.contains(&idx) {
                        idxs.push(idx);
                    }
                }
            }
        }
        if idxs.len() >= 2 {
            part.section_bytes_batch_for(owner, &idxs, META_PREFAULT_GAP, &|_| true).map_err(read_err)?;
        }
        Ok(())
    }

    /// sortpath (code-keyed zones): `face`'s CodeZones body in part `p` and the dictionary its codes index, read once per
    /// generation; `None` where the part sealed none, or the dictionary's section count does not match the body (a
    /// body of another part's geometry never answers).
    fn code_zones(&self, p: usize, face: u32, part: &Arc<OpenPart>) -> PgResult<Option<&(ZoneBody, DictHandle)>> {
        if let Some(c) = self.held_codes.get(p, face) {
            return Ok(c.zones.as_ref());
        }
        // D4410: the list's warm cell, else read here (and kept warm while the list keeps any); the statement holds its own
        let cell = &self.list.faces.lane(face).codes[p];
        if let Some(c) = cell.get() {
            // cache plan C2 / C5: as `adopt_warm`
            if let Some((_, d)) = &c.zones {
                d.note_reads();
            }
            return Ok(self.held_codes.publish(p, face, c).zones.as_ref());
        }
        // FASTADD: a column the part lacks: its stand-in's sections and dictionary
        let part = self.lane(p, face, part)?;
        let c = match self.list.faces.locate(face, p, part)? {
            Some((attno, ord)) => match section_body(part, SectionKind::CodeZones, attno, ord)? {
                Some(body) if pgrc_format::meta::codezones_len(&body) == Some(part.footer().granule_count as usize + 1) => {
                    let binding = pgrc_am::scan::codec_binding();
                    let d = DictHandle::open(Arc::clone(part), None, binding.unwrappers, attno, ord).map_err(read_err)?;
                    Some((ZoneBody::new(body), d))
                }
                _ => None,
            },
            None => None,
        };
        let c = Arc::new(CodesEntry::new(c)); // alloc: Exec
        let c = if held::warm(&self.list) {
            // charged before the put, as `DictEntry::warm`
            c.warm();
            cell.put(c)
        } else {
            c
        };
        Ok(self.held_codes.publish(p, face, c).zones.as_ref())
    }

    fn code_zones_if_read(&self, p: usize, face: u32) -> Option<&(ZoneBody, DictHandle)> {
        self.held_codes.get(p, face)?.zones.as_ref()
    }

    fn meta_if_read(&self, p: usize, face: u32) -> Option<&MetaBodies> {
        self.face_bodies_if_read(p, face)?.meta.get()
    }

    fn body_if_read(&self, p: usize, face: u32) -> Option<&[u8]> {
        let lane = self.list.faces.lane(face);
        if let Some(StatsBody::Owned(b)) = lane.stats[p].get() {
            return Some(b);
        }
        self.face_bodies_if_read(p, face)?.body.get().and_then(|b| b.as_deref())
    }

    fn part_face(&self, p: usize, face: u32) -> Option<&[u8]> {
        if let Some(b) = self.body_if_read(p, face) {
            return Some(b);
        }
        match self.list.faces.lane(face).stats[p].get()? {
            StatsBody::Catalog => self.list.face_record(p, face),
            StatsBody::Owned(b) => Some(b),
            StatsBody::None => None,
        }
    }

    /// Code lane `face`'s dictionary of part `p` (the writer's, else the verbatim part's ranks), opened once per generation.
    /// A shred lane's is the writer's only where the part sealed its codes executable (`DictHandle::exec_form`: the
    /// zero-null `DICT_EXEC` or the nullable twin, M6 P-a); a part that sealed them storage-only publishes its ranks.
    fn dict(&self, p: usize, face: u32, part: &Arc<OpenPart>, cur: &mut StreamCursor<'static>, scratch: &mut crate::dict::text_hosts::TextScratch, touch: Option<DictTouch>, keep: bool) -> PgResult<&DictEntry> {
        if let Some(d) = self.dict_if_open(p, face).or_else(|| self.adopt_warm(p, face, keep)) {
            return Ok(d);
        }
        let goes = self.goes(keep);
        let cell = &self.list.faces.lane(face).dicts[p];
        // D3063 (2a): and at the next dictionary it opens (a dictionary's images are faulted block by block as it is read)
        if let Some(e) = mcx::governor::refusal("pgrc part cache") {
            return Err(Box::<PgError>::new(e)); // alloc: Cold
        }
        let binding = pgrc_am::scan::codec_binding();
        // FASTADD: a column the part lacks: its stand-in's dictionary
        let part = self.lane(p, face, part)?;
        let (attno, ord) = self.list.faces.locate(face, p, part)?.ok_or_else(|| err("gravity: a dictionary of a shred lane the part did not elect"))?;
        let coded = if cur.entry().encoding == EncodingId::DictCodes as u16 {
            let d = match touch {
                Some(t) => DictHandle::open_touch(Arc::clone(part), None, binding.unwrappers, attno, ord, t),
                None => DictHandle::open(Arc::clone(part), None, binding.unwrappers, attno, ord),
            }
            .map_err(read_err)?; // alloc: Exec
            (ord == 0 || d.exec_form().is_some()).then_some(d)
        } else {
            None
        };
        let entry = if let Some(d) = coded {
            DictEntry::new(Dict::Part(d))
        } else {
            let table = crate::dict::text_hosts::row_codes(part, cur, stream_key(attno, ord), scratch, None).map_err(table_err)?;
            DictEntry::new(Dict::Ranked { table: crate::dict::text_hosts::FpPin::of(&table), epoch: part.dict_epoch(attno, ord) })
        };
        // D2985: a streaming statement keeps its dictionaries warm in the list's cells for the next statement while the
        // process part cache, whose census counts their images, is under its budget; past it only in its own cells.
        // D3823: so does a uses-only release whose breaker keeps code words. The other statements always keep them warm.
        let entry = Arc::new(entry); // alloc: Exec
        let shared = goes.is_none_or(|sd| sd.shares());
        let entry = if shared && held::warm(&self.list) {
            // charged before the put: when another statement's entry is already kept there, ours drops in it, its charge first
            DictEntry::warm(&entry);
            cell.put(entry)
        } else {
            entry
        };
        Ok(match goes {
            Some(sd) => sd.set(p, face, entry, !shared, true),
            None => self.held.publish(p, face, entry),
        })
    }

    /// D2985 / D3823: a streaming statement's dictionaries, and a code-word breaker's but for a lane that renders through a
    /// derived table (`keep`), go with their part (its `StmtDicts` cells); the others it keeps to its end (`held`). D4410:
    /// either way the statement holds its own `Arc`, the list's warm cell at most one more.
    fn goes(&self, keep: bool) -> Option<&release::StmtDicts> {
        self.stmt.as_deref().filter(|sd| sd.owns_dicts() && (sd.releases_dicts() || !keep))
    }

    /// D4410: part `p`'s face `face` from the list's warm cell (another statement opened it), held by this statement from
    /// now on as `dict` holds what it opens; `None` while the cell is empty. A reader asks here where it borrowed the
    /// list's cell before (`dict_at`), so a warm dictionary is never read or staged again.
    fn adopt_warm(&self, p: usize, face: u32, keep: bool) -> Option<&DictEntry> {
        let e = self.list.faces.lane(face).dicts[p].get()?;
        // cache plan C2 / C5: the statement reads the cell's images through the list's handle, past the part cache's doors
        if let Dict::Part(d) = &e.dict {
            d.note_reads();
        }
        Some(match self.goes(keep) {
            Some(sd) => sd.set(p, face, e, false, false),
            None => self.held.publish(p, face, e),
        })
    }

    /// The statement's own entry of part `p`'s face `face` (`StmtDicts` cells, else `held`); D4410: never the list's warm
    /// cell, which the part cache's evicted hook may empty while the statement runs (`adopt_warm` takes it from there).
    #[inline]
    fn dict_if_open(&self, p: usize, face: u32) -> Option<&DictEntry> {
        match self.stmt.as_deref().and_then(|sd| sd.get(p, face)) {
            Some(d) => Some(d),
            None => self.held.get(p, face),
        }
    }
}

/// V4R-310 (D840's one predicate): part `spec`'s dictionary entries stand for visible rows of the statement, so an entries
/// read (dict.entries, code.heads) may take them: no delete vector (pgrc's `part_clean` joins here once tombstone-bearing
/// tables are served), and no transaction snapshot (D839: an RR writer's chained generation). Else the part's rows.
pub fn entries_safe(spec: &PartSpec, r: &Resolved) -> bool {
    spec.deletes.is_none() && !r.xact_snapshot
}

/// sortmerge T2 (merged.rs): whether part `p` of `recipe`'s relation sealed a sort witness (its SortKey section) whose
/// leading entries are the columns `keys` in that order, each ascending, NULLS LAST, C-collated and no jsonb path: the
/// order the seal's cluster sort put the part's rows in (pgrc_write seal.rs `cluster_sort`, which verifies it).
pub(crate) fn part_sorted_on(resolved: &Resolved, recipe: &GranuleRecipe, p: usize, keys: &[i16]) -> PgResult<bool> {
    use pgrc_format::class::CollationClass;
    use pgrc_format::sortkey::{NullsOrder, SortDir, SortKeyRecord};
    let rp = resolved.rel(recipe.rel).ok_or_else(|| err("gravity: a merged source bound without its part list"))?;
    let part = rp.part(p, &recipe.registry)?;
    let Some(body) = section_body(part, SectionKind::SortKey, 0, 0)? else { return Ok(false) };
    let rec = SortKeyRecord::decode(&body).map_err(|_| err("gravity: a pgrc part's SortKey record does not decode"))?;
    let lead = |(e, &a): (&pgrc_format::sortkey::SortKeyEntry, &i16)| {
        a > 0 && e.attno == a as u32 && e.dir == SortDir::Asc as u8 && e.nulls == NullsOrder::Last as u8 && e.collation_class == CollationClass::C.as_u8() && e.pad == 0
    };
    Ok(rec.keys.len() >= keys.len() && rec.keys.iter().zip(keys.iter()).all(lead))
}

/// PS1-05 (road `nest.eagersafe`): an open-time zone predicate `lane op <key cell>` (src.verdicts' key-cell slot,
/// `word_zone_key_arg`): the source's open stores an eager-safe InitPlan's value in key cell `cell`, of typlen `len`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyZone {
    pub lane: u16,
    pub op: ZoneOp,
    pub len: u8,
    pub cell: u16,
}

impl KeyZone {
    /// The predicate over this seek's key cells, read as verdict_zone.rs reads a literal of the same length; none for a
    /// NULL value (no row of a NULL comparison passes, and none is a verdict) or a cell past the seek's.
    pub fn pred(&self, keys: &[Word], keys_null: &[u8]) -> Option<ZonePred> {
        let c = self.cell as usize;
        let (Some(&x), Some(0)) = (keys.get(c), keys_null.get(c)) else { return None };
        let value = match self.len {
            1 => x as u8 as i64,
            2 => x as i16 as i64,
            4 => x as i32 as i64,
            8 => x as i64,
            _ => return None,
        };
        Some(ZonePred::word(self.lane, self.op, value))
    }
}

/// sortpath (code-keyed zones): the text constant of prefix predicate `lane op key` (SRC-09's key of `image`, a bound
/// one's or an IN list's bound member), which a part with code zones (`SectionKind::CodeZones`) places in its byte-rank
/// sorted dictionary, so the compare is decided on codes exactly where the 8-byte prefix keys tie.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeImage {
    pub lane: u16,
    pub op: ZoneOp,
    pub key: i64,
    pub image: Box<[u8]>,
}

/// A word-valued predicate's value from its bind word `x` of typlen `len`, as verdict_zone.rs reads a literal of the same
/// length (decompose `zone_word_of`: 1 byte zero-extended, 2 / 4 sign-extended; a zero-extended oid never binds).
pub fn word_value(x: u64, len: u8) -> Option<i64> {
    Some(match len {
        1 => x as u8 as i64,
        2 => x as i16 as i64,
        4 => x as i32 as i64,
        8 => x as i64,
        _ => return image_key(x, len),
    })
}

/// NEW-72: the zone value of a by-reference constant's word `x` in bound form `form` (program zone.rs `BOUND_*`): SRC-09's
/// prefix key of a text payload, P5-13's order key of a fixed image, or either's probe word (P5-11 / P5-13 `HashEq`,
/// `HashIn`), made from the execution's image as the lowering made them from a literal's; `None` for another form or an
/// image the host does not read in place (external, compressed).
pub fn image_key(x: u64, form: u8) -> Option<i64> {
    use gravity_decompose::ops::scan::{eq_bloom_key, fixed_order_key, text_prefix_key};
    use gravity_program::{BOUND_FIXED_HASH, BOUND_FIXED_PREFIX, BOUND_TEXT_HASH, BOUND_TEXT_PREFIX};
    let fixed = (form & 0x3f) as usize;
    match form {
        BOUND_TEXT_PREFIX => super::raw::with_bind_image(x, None, text_prefix_key),
        BOUND_TEXT_HASH => super::raw::with_bind_image(x, None, eq_bloom_key),
        _ if fixed == 0 || fixed > 16 => None,
        _ if form & 0xc0 == BOUND_FIXED_PREFIX => super::raw::with_bind_image(x, Some(fixed), fixed_order_key),
        _ if form & 0xc0 == BOUND_FIXED_HASH => super::raw::with_bind_image(x, Some(fixed), eq_bloom_key),
        _ => None,
    }
}

/// sortpath (code-keyed zones): the payload of text bind word `x` (a varlena the host reads in place), `None` for a
/// NULL, external or compressed one.
pub fn text_image(x: u64) -> Option<Box<[u8]>> {
    super::raw::with_bind_image(x, None, |d: &[u8]| Box::<[u8]>::from(d))
}

/// NEW-72: `statics` (a site's static zone predicates) with each bound one's value from `bound` (`(index, value)`), or
/// `None` when a bound one has no value (no protocol, a contested cell, a slot past the numbering): that execution takes
/// no zone verdict over the site.
pub fn bind_statics(statics: &mut [ZonePred], bound: &[(u16, i64)]) -> Option<()> {
    let mut seen = vec![false; statics.len()]; // alloc: Exec
    // bounded("other: one bound value per zone predicate of the statement's own quals: the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
    for &(at, value) in bound {
        if let Some(z) = statics.get_mut(at as usize).filter(|z| z.bound) {
            z.value = value;
            seen[at as usize] = true;
        }
    }
    statics.iter().zip(seen.iter()).all(|(z, &s)| !z.bound || s).then_some(())
}

/// A word-valued zone predicate's op (`ZoneOp` ordinal, `word_zone_arg`); `NotNull` never takes a word.
fn zone_op_of(op: u8) -> Option<ZoneOp> {
    [ZoneOp::Lt, ZoneOp::Le, ZoneOp::Eq, ZoneOp::Ge, ZoneOp::Gt].get(op as usize).copied().or((op == ZoneOp::Ne as u8).then_some(ZoneOp::Ne))
}

/// A scan site's host verdicts for one execution (`ProtoKind::Verdicts`, V4R-4): exec's protocol writes them at
/// prepare, every source of the site reads them at bind (the execution's own cell, from its `Resolved`: D300). `LEGACY`
/// enables = no protocol declared: the zone verdicts alone, as before v4. The cell is the Admitted's (BIND), so every
/// execution of a cached plan meets it (the runner's Bind keeps one Admitted per plan; the simple protocol admits each
/// statement), and two of them can be live at once in a session (two portals over one prepared statement): src.verdicts
/// claims the cell at prepare (D289, `claim`), only the one live claimant writes it, a second live claim contests it
/// (cleared: both then read LEGACY, never the other's state), and the last claim's end clears it.
pub struct VerdictCell {
    words: [AtomicU64; 5],
    /// D294: this execution's runtime filters, each armed after its build by its rt claim's owner (`claim_rt_at`) and
    /// cleared with its last rt claim, so no later execution of the Admitted reads it. D3245: one per slot, `RT_SLOTS` a site.
    rt: Mutex<[Option<Arc<RtFilter>>; RT_SLOTS]>,
    /// D289: this execution's word-valued zone predicates (`set_word_zone`), as many as the site declares.
    zone: Mutex<Vec<ZonePred>>,
    /// PS1-05: this execution's open-time zone predicates (`set_word_zone`'s key-cell slots), each read at the seek.
    key_zone: Mutex<Vec<KeyZone>>,
    /// NEW-72: this execution's values of the site's bound static predicates (`set_word_zone`'s `WORD_ZONE_AT` slots):
    /// `(static index, value)` each.
    bound: Mutex<Vec<(u16, i64)>>,
    /// sortpath (code-keyed zones): this execution's text constants of its prefix predicates (`CodeImage`), each source's
    /// key to the parts' dictionaries.
    images: Mutex<Vec<CodeImage>>,
    /// The claims on the verdicts (`words`, `zone`) and, apart (one execution claims both), on `rt` (D294).
    held: Held,
    rt_held: [Held; RT_SLOTS],
    /// D2856 (colskip): the runtime filters' generation, moved by every `arm_rt_at`, `clear_rt_at` and rt `clear`: a source
    /// that took the filters under an older one takes the cell's again at its next part (a rerun set's build ran again).
    rt_gen: AtomicU64,
    /// D2856: each slot's measuring window a source leaves at its unbind and the site's next bind starts from (a rerun
    /// member run as a section binds its sources again at every run): one window may span a rerun set's runs. Cleared with
    /// its filter.
    rt_win: Mutex<[RtWindow; RT_SLOTS]>,
}

/// D3245: the runtime filters one scan site holds at once (exec's join.rtfilter: the site's k-th declared filter claims
/// slot k; a filter past the last claims none). A granule or part is skipped when any armed filter proves it holds no key
/// of its build: each filter's probe drops its unmatched rows (verdict.rtconst's `accepts`) and the ops before it are
/// raise-free (D237), so each filter's skip is valid alone and any of them is. CHOSEN: a star join's dimensions over one
/// fact scan (date, item, store, customer).
pub const RT_SLOTS: usize = 4;

/// D2856 (colskip): the runtime filter's measuring window (`rt_count`) as a source leaves it: its checks and cuts so far,
/// idle, and the granules since its last sampled check. A re-armed filter keeps it (it is never a verdict: the window only
/// decides how often the filter is asked).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtWindow {
    pub seen: u32,
    pub cut: u32,
    pub idle: bool,
    pub rest: u32,
}

/// D3245: one runtime-filter slot as a source holds it: its lane once armed (`usize::MAX` before), off for the bind (no
/// matching lane), idle (its last window cut under one in `rt_div()`: `rt_count`) with the granules since its last sampled
/// check, its measuring window's checks / cuts, and the filter `rt_arm` took (D294: the cell's is cleared when the
/// execution ends; D4656: the source's reference goes at its unbind).
#[derive(Clone)]
struct RtSlot {
    lane: usize,
    off: bool,
    idle: bool,
    rest: u32,
    seen: u32,
    cut: u32,
    f: Option<Arc<RtFilter>>,
}

impl RtSlot {
    const NONE: RtSlot = RtSlot { lane: usize::MAX, off: false, idle: false, rest: 0, seen: 0, cut: 0, f: None };

    fn from_window(w: RtWindow) -> RtSlot {
        RtSlot { idle: w.idle, rest: w.rest, seen: w.seen, cut: w.cut, ..RtSlot::NONE }
    }

    fn window(&self) -> RtWindow {
        RtWindow { seen: self.seen, cut: self.cut, idle: self.idle, rest: self.rest }
    }

    /// Armed on a lane of the site for this bind.
    fn armed(&self) -> bool {
        self.lane != usize::MAX && !self.off
    }

    /// PS1-22: a text key's filter (its parts' dictionaries answer, its checks count per part only).
    fn text(&self) -> bool {
        self.f.as_deref().is_some_and(|f| !f.fps.is_empty())
    }

    fn measuring(&self) -> bool {
        self.armed() && !self.idle
    }
}

/// The live claims on one part of a verdict cell and its owner's token (0: none; `CONTESTED`: two were live at once).
#[derive(Default)]
struct Held {
    live: AtomicU64,
    owner: AtomicU64,
}

/// Claim tokens: one per claim, never reused.
static CLAIMS: AtomicU64 = AtomicU64::new(1);
/// Claims that found another live on the same cell (the census: `contested_census`).
static CONTESTED: AtomicU64 = AtomicU64::new(0);

/// Claims that contested a verdict cell since the process started (two live executions of one Admitted).
pub fn contested_census() -> u64 {
    CONTESTED.load(Ordering::Relaxed)
}

/// An execution's claim on a site's verdict cell (`VerdictCell::claim`), held by its protocols' state until the execution
/// ends; the last live claim's drop clears the cell.
pub struct VerdictClaim {
    cell: Arc<VerdictCell>,
    token: u64,
    /// A claim on runtime-filter slot `k` (`claim_rt_at`): `Some(k)`, else on the verdicts (`claim`).
    rt: Option<usize>,
}

impl VerdictClaim {
    /// This execution may write its part of the cell: the one live claimant, not contested since.
    pub fn owner(&self) -> bool {
        self.cell.held(self.rt).owner.load(Ordering::Acquire) == self.token
    }
}

impl Drop for VerdictClaim {
    fn drop(&mut self) {
        let held = self.cell.held(self.rt);
        if held.live.fetch_sub(1, Ordering::AcqRel) == 1 {
            held.owner.store(0, Ordering::Release);
            self.cell.clear(self.rt);
        }
    }
}

/// `join.rtfilter` (H07c): a finished build's keys armed on its probe's scan site for one execution. A granule or part whose
/// zone range on column `attno` holds no build key (outside `[lo, hi]`, or between the runs of a build within the cap) is
/// skipped unread; a row is never filtered, the probe still tests it. `lo > hi` = the build is empty: every granule is
/// skipped, as PostgreSQL's hash join never scans its outer side over an empty inner one.
pub struct RtFilter {
    pub attno: i16,
    pub lo: i64,
    pub hi: i64,
    /// D3245: the build's keys folded into maximal runs of consecutive keys `(first, last)`, ascending and disjoint (a key
    /// set: runs of one key); empty = the range alone.
    pub runs: memledger::ChargedVec<(i64, i64)>,
    /// PS1-22: a text key's build (no range: `lo`, `hi` span every word, `runs` empty): its keys' `Fp128` images (lo word |
    /// hi word << 64), ascending, deduplicated. A part is skipped only when none of its dictionary entries' is among them
    /// (verdicts.rs `rt_text_cuts`); empty = an integer key's filter.
    pub fps: memledger::ChargedVec<u128>,
}

impl RtFilter {
    /// D4533: a filter whose own runs and images are copies charged to `owner`, the build's slot owner and kind (as
    /// agg hosts.rs `admit_set` charges its kept set; `None`: uncharged). Its bytes leave with the filter, when its slot
    /// is cleared or armed again. `None` when a copy is refused: no filter, so the probe reads every row.
    pub fn charged(attno: i16, lo: i64, hi: i64, runs: &[(i64, i64)], fps: &[u128], owner: Option<(memledger::Owner, memledger::Kind)>) -> Option<RtFilter> {
        let (o, k) = (owner.map(|o| o.0), owner.map_or(memledger::Kind::Region, |o| o.1));
        let mut r = memledger::ChargedVec::new(o, k); // alloc: Exec charged o
        r.extend_from_slice(runs).ok()?; // alloc: Exec charged o
        let mut f = memledger::ChargedVec::new(o, k); // alloc: Exec charged o
        f.extend_from_slice(fps).ok()?; // alloc: Exec charged o
        Some(RtFilter { attno, lo, hi, runs: r, fps: f })
    }

    /// D3245: ascending, deduplicated `keys` folded into maximal runs of consecutive keys, under `step` (a build's keys:
    /// a tick per `BATCH`).
    pub fn runs_of(step: &mut Step, keys: &[i64]) -> core::result::Result<Vec<(i64, i64)>, cancelpoll::Canceled> {
        let mut runs: Vec<(i64, i64)> = Vec::new(); // alloc: Exec
        for (i, &k) in keys.iter().enumerate() {
            step.batch(i)?;
            match runs.last_mut() {
                Some(r) if r.1.checked_add(1) == Some(k) => r.1 = k,
                _ => runs.push((k, k)), // alloc: Exec
            }
        }
        Ok(runs)
    }

    /// Whether no run meets `[min, max]` (there are runs: the filter is not the range alone).
    pub fn misses(&self, min: i64, max: i64) -> bool {
        !self.runs.is_empty() && self.runs.get(self.runs.partition_point(|&(_, last)| last < min)).is_none_or(|&(first, _)| first > max)
    }
}

impl VerdictCell {
    pub const LEGACY: u64 = u64::MAX;

    /// The owner token of a cell two live claims contested.
    const CONTESTED: u64 = u64::MAX;

    fn new() -> VerdictCell {
        VerdictCell { words: [AtomicU64::new(VerdictCell::LEGACY), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)], rt: Mutex::new(Default::default()), zone: Mutex::new(Vec::new()), key_zone: Mutex::new(Vec::new()), bound: Mutex::new(Vec::new()), images: Mutex::new(Vec::new()), held: Held::default(), rt_held: Default::default(), rt_gen: AtomicU64::new(0), rt_win: Mutex::new([RtWindow::default(); RT_SLOTS]) } // alloc: Exec
    }

    /// An execution's claim on the verdicts at prepare (src.verdicts' slot 0: one per site and execution). The first live
    /// claim owns them; one taken while another is live contests them: cleared, and owned by no claimant until every
    /// claim has ended (the verdicts only skip and narrow, so LEGACY is always correct).
    pub fn claim(self: &Arc<Self>) -> VerdictClaim {
        self.claim_part(None)
    }

    /// D294: an execution's claim on the runtime filter of slot 0 (`claim_rt_at`).
    pub fn claim_rt(self: &Arc<Self>) -> VerdictClaim {
        self.claim_rt_at(0)
    }

    /// D294: an execution's claim on runtime-filter slot `k` (join.rtfilter's prepare), as `claim` on the verdicts: only
    /// the one live claimant arms it, a second live claim contests it (no filter in that slot for either), the last claim's
    /// end clears it. D3245: each slot is claimed apart.
    pub fn claim_rt_at(self: &Arc<Self>, k: usize) -> VerdictClaim {
        self.claim_part(Some(k.min(RT_SLOTS - 1)))
    }

    fn claim_part(self: &Arc<Self>, rt: Option<usize>) -> VerdictClaim {
        let token = CLAIMS.fetch_add(1, Ordering::Relaxed);
        let held = self.held(rt);
        if held.live.fetch_add(1, Ordering::AcqRel) == 0 {
            // owned only from none: a contest or a last drop still in flight leaves this claim unowned (LEGACY)
            let _ = held.owner.compare_exchange(0, token, Ordering::AcqRel, Ordering::Acquire);
        } else {
            held.owner.store(VerdictCell::CONTESTED, Ordering::Release);
            CONTESTED.fetch_add(1, Ordering::Relaxed);
            self.clear(rt);
        }
        VerdictClaim { cell: Arc::clone(self), token, rt }
    }

    fn held(&self, rt: Option<usize>) -> &Held {
        match rt {
            Some(k) => &self.rt_held[k],
            None => &self.held,
        }
    }

    /// Arms runtime-filter slot 0 (`arm_rt_at`).
    pub fn arm_rt(&self, f: RtFilter) {
        self.arm_rt_at(0, f);
    }

    /// Arms the site's runtime filter of slot `k` for this execution (its rt claim's owner, after each run of its build).
    /// D2856 (colskip): a build a rerun set runs again arms again, and its filter replaces the last run's (the generation
    /// moves, so every source of the site takes the filters again at its next part): a probe never skips by an earlier
    /// run's keys.
    pub fn arm_rt_at(&self, k: usize, f: RtFilter) {
        let mut g = self.rt.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = g.get_mut(k) {
            *s = Some(Arc::new(f)); // alloc: Exec
        }
        self.rt_gen.fetch_add(1, Ordering::AcqRel);
    }

    /// Clears runtime-filter slot 0 (`clear_rt_at`).
    pub fn clear_rt(&self) {
        self.clear_rt_at(0);
    }

    /// D2856 (colskip): no runtime filter in slot `k` until its build arms one again: a rerun set re-opened over the build
    /// (its run starts), or a run of the build that gives none (a spilled build, a region the reader does not take). The
    /// window stays (`rt_win`).
    pub fn clear_rt_at(&self, k: usize) {
        let mut g = self.rt.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = g.get_mut(k) {
            *s = None;
        }
        self.rt_gen.fetch_add(1, Ordering::AcqRel);
    }

    /// This execution's runtime filter of slot 0, if armed (each source keeps its own reference from `rt_arm`).
    pub fn rt(&self) -> Option<Arc<RtFilter>> {
        self.rt.lock().unwrap_or_else(|e| e.into_inner())[0].clone()
    }

    /// D2856: the runtime filters' generation (`arm_rt_at` / `clear_rt_at` move it) and every slot's filter of that
    /// generation, read under one lock.
    pub fn rt_at(&self) -> (u64, [Option<Arc<RtFilter>>; RT_SLOTS]) {
        let g = self.rt.lock().unwrap_or_else(|e| e.into_inner());
        (self.rt_gen.load(Ordering::Acquire), g.clone())
    }

    /// D2856: the runtime filters' generation alone (a source's check at each part: one load, no lock).
    pub fn rt_gen(&self) -> u64 {
        self.rt_gen.load(Ordering::Acquire)
    }

    /// D2856: the measuring window of slot 0 (`rt_window_at`).
    pub fn rt_window(&self) -> RtWindow {
        self.rt_window_at(0)
    }

    /// D2856: the measuring window of slot `k` the site's last unbound source left (a fresh one before any).
    pub fn rt_window_at(&self, k: usize) -> RtWindow {
        self.rt_win.lock().unwrap_or_else(|e| e.into_inner()).get(k).copied().unwrap_or_default()
    }

    /// D2856: a source's measuring window of slot `k` at its unbind, for the site's next bind in this execution.
    pub fn set_rt_window_at(&self, k: usize, w: RtWindow) {
        if let Some(s) = self.rt_win.lock().unwrap_or_else(|e| e.into_inner()).get_mut(k) {
            *s = w;
        }
    }

    /// D289: the site's word-valued zone predicates (`lane op $n`, a generic plan's key word; src.verdicts' slots
    /// `VERDICT_WORD_ZONE..`, each `(arg, word)`) with this execution's values, written at every prepare (the cell is the
    /// Admitted's, reused by each execution of a cached plan). A NULL word, an unknown op or length gives none: the
    /// predicate is not a verdict, never a guess.
    /// PS1-05: a key-cell slot (`word_zone_key_arg`, an eager-safe InitPlan's value the source's open stores) is kept as
    /// its lane, op, length and cell (`key_zone`): its value is the seek's (`KeyZone::pred`).
    /// NEW-72: a `WORD_ZONE_AT` slot's value is the bound static predicate's (`bound`); a `WORD_ZONE_LIST` slot's
    /// predicates are exec's to add (`add_word_zone`: it walks the array).
    pub fn set_word_zone(&self, preds: impl Iterator<Item = (u64, usize)>, words: &[u64], nulls: &[u8]) {
        let mut zone = self.zone.lock().unwrap_or_else(|e| e.into_inner());
        let mut key_zone = self.key_zone.lock().unwrap_or_else(|e| e.into_inner());
        let mut bound = self.bound.lock().unwrap_or_else(|e| e.into_inner());
        let mut images = self.images.lock().unwrap_or_else(|e| e.into_inner());
        zone.clear();
        key_zone.clear();
        bound.clear();
        images.clear();
        // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
        for (arg, w) in preds {
            let (lane, op, len) = word_zone_of(arg);
            if word_zone_list_of(arg).is_some() {
                continue;
            }
            if let Some(cell) = word_zone_key_of(arg) {
                if let (Some(op), true) = (zone_op_of(op), [1, 2, 4, 8].contains(&len)) {
                    key_zone.push(KeyZone { lane, op, len, cell }); // alloc: Exec
                }
                continue;
            }
            let (Some(&x), Some(0)) = (words.get(w), nulls.get(w)) else { continue };
            let Some(value) = word_value(x, len) else { continue };
            if let Some(at) = word_zone_at_of(arg) {
                bound.push((at, value)); // alloc: Exec
                // sortpath: a text prefix predicate's constant, for the parts' code zones
                if let (gravity_program::BOUND_TEXT_PREFIX, Some(op)) = (len, zone_op_of(op)) {
                    if let Some(image) = super::raw::with_bind_image(x, None, |d| Box::<[u8]>::from(d)) {
                        images.push(CodeImage { lane, op, key: value, image }); // alloc: Exec
                    }
                }
            } else if let Some(op) = zone_op_of(op) {
                zone.push(ZonePred::word(lane as u16, op, value)); // alloc: Exec
            }
        }
    }

    /// NEW-72: this execution's IN-list predicates (exec's walk of each `WORD_ZONE_LIST` slot's array: `list_preds`),
    /// after the word-valued ones `set_word_zone` wrote.
    /// Each is kept once: a duplicate of a word-valued one (`set_word_zone`'s, a few) or of an earlier list's is left out
    /// (`list_preds` gives a list's members once each), so bind appends them with no scan of its own.
    pub fn add_word_zone(&self, preds: &[ZonePred]) {
        let mut zone = self.zone.lock().unwrap_or_else(|e| e.into_inner());
        let n = zone.len();
        let mut seen: std::collections::HashSet<ZonePred> = std::collections::HashSet::new(); // alloc: Exec
        // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
        for z in preds {
            if !zone[..n].contains(z) && seen.insert(*z) {
                zone.push(*z); // alloc: Exec
            }
        }
    }

    /// sortpath (code-keyed zones): the text constants of this execution's IN-list prefix predicates (exec's `list_preds`),
    /// after the bound ones `set_word_zone` wrote.
    pub fn add_code_images(&self, more: Vec<CodeImage>) {
        self.images.lock().unwrap_or_else(|e| e.into_inner()).extend(more);
    }

    /// sortpath: this execution's text constants of its prefix predicates; none without a protocol.
    pub fn code_images(&self) -> std::sync::MutexGuard<'_, Vec<CodeImage>> {
        self.images.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// NEW-72: this execution's bound static values (`set_word_zone`); none without a protocol.
    pub fn bound(&self) -> std::sync::MutexGuard<'_, Vec<(u16, i64)>> {
        self.bound.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// This execution's word-valued zone predicates; none without a protocol (LEGACY: the static predicates alone).
    pub fn word_zone(&self) -> std::sync::MutexGuard<'_, Vec<ZonePred>> {
        self.zone.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// PS1-05: this execution's open-time zone predicates (`set_word_zone`'s key-cell slots); none without a protocol.
    pub fn key_zone(&self) -> std::sync::MutexGuard<'_, Vec<KeyZone>> {
        self.key_zone.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// LEGACY again (the zone verdicts alone), or no runtime filter (`rt`): the last claim of that part ended, or a
    /// second execution contested it.
    fn clear(&self, rt: Option<usize>) {
        if let Some(k) = rt {
            self.rt.lock().unwrap_or_else(|e| e.into_inner())[k] = None;
            self.rt_gen.fetch_add(1, Ordering::AcqRel);
            self.rt_win.lock().unwrap_or_else(|e| e.into_inner())[k] = RtWindow::default();
            return;
        }
        self.words[0].store(VerdictCell::LEGACY, Ordering::Release);
        // bounded("small-fixed: the cell's verdict words past the first, 4")
        for w in &self.words[1..] {
            w.store(0, Ordering::Relaxed);
        }
        self.zone.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.key_zone.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.bound.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.images.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Word 0 = the enables (`VERDICT_*`), 1 / 2 = the condition fingerprint's low / high word, 3 = the late lanes, 4 = the
    /// packed candidates.
    pub fn set(&self, i: usize, w: u64) {
        if let Some(c) = self.words.get(i) {
            c.store(w, Ordering::Release);
        }
    }

    pub fn get(&self) -> (u64, [u64; 2]) {
        (self.words[0].load(Ordering::Acquire), [self.words[1].load(Ordering::Acquire), self.words[2].load(Ordering::Acquire)])
    }

    /// decode.late's lanes (bit k = site lane k), read with `VERDICT_LATE`.
    pub fn late(&self) -> u64 {
        self.words[3].load(Ordering::Acquire)
    }

    /// decode.packed's candidate lanes, read with `VERDICT_PACKED`.
    pub fn packed(&self) -> u64 {
        self.words[4].load(Ordering::Acquire)
    }
}

#[derive(Clone)]
pub struct GranuleRecipe {
    pub rel: Oid,
    pub natts: usize,
    pub attnos: Vec<i16>,
    /// The physical width of each lane (2 / 4 / 8): the source decodes int2 /
    /// int4 lanes typed and publishes them at that width (§1.10 increment 3).
    pub widths: Vec<u8>,
    /// Per lane, `KIND_BYVAL` (an int lane) or `KIND_CODES` (a dictionary text
    /// column published as its code lane, step 4).
    pub kinds: Vec<u8>,
    /// The site's table lanes (expr.md §2.2): lane `natts + 1 + i` (after the offset lane) is `tables[i]`.
    pub tables: Arc<[crate::dict::text_hosts::TableSpec]>,
    /// One code lane whose dictionary-coded, delete-free parts publish their entries instead of
    /// their rows (`count(DISTINCT text)` alone: breakers.md "the dictionary union").
    pub entries: Option<usize>,
    /// PS1-19 (`with_entries_every`): every lane (all code lanes, `entries` = lane 0) publishes its entries side by side
    /// in one window, each lane invalid past its own count (`count(DISTINCT a), count(DISTINCT b)`: each a lane's set alone).
    pub entries_every: bool,
    /// PS1-18: the program filters the entries window's entries: a part is never taken whole as entry runs (`entry_runs_offer`),
    /// which would skip the filter.
    pub entries_filtered: bool,
    /// RD-dict.entries (`with_entries_nulls`): the declarer keeps NULL as a value of its set (`SELECT DISTINCT x`), so a
    /// part whose entries lane holds a NULL publishes one invalid row after its entries (`stage_entries`); never entry runs.
    pub entries_nulls: bool,
    /// D2981 (`with_entries_desc`): the declarer is a descending top over the entries lane, so each part publishes its
    /// codes from its highest down (window 0 the highest, each window's codes descending; `stage_entries`).
    pub entries_desc: bool,
    /// The code lanes' dictionaries open for point probes (block faults); a lane a table is built over opens dense.
    pub sparse_dicts: bool,
    /// P5-18: the renders a code lane of the site can get at most (`with_dict_renders`): a bound, `u64::MAX` for none, 0 for a
    /// site that declares its probes and never streams.
    pub dict_renders: u64,
    /// A worker faults dict frame 0 of every part it enters (the phrase-heads recipes read the first
    /// entries of every part); a top-k render needs arbitrary frames, so it leaves this off.
    pub head_prefault: bool,
    /// A filter-only lane whose own zone predicates are `AllMatch` over a granule stays undecoded even when the granule is `Mixed` on another lane (the kernel skips its predicates: `FLAG_LANE_ALL_MATCH`).
    pub lane_elide: bool,
    /// Per lane, a fixed lane's `ncodes_or_scale` (a shredded number path's scale); a code lane's count overrides it.
    pub scales: Vec<u32>,
    /// The `numeric(p, s)` lanes: `KIND_FIXED` width 8, the packed mantissa at the typmod scale (item 24); a verbatim part's granule `KIND_BYREF` as `lane_of_class` would.
    pub numeric: LaneBits,
    /// N14: the `numeric` lanes of an unconstrained numeric column read as its packed codes (decompose `packed_dyn_lanes`,
    /// `from_plan_lanes`): each stream's mantissas at the scale its chunk elected (the stream entry's `aux32`, published in
    /// the lane's `ncodes_or_scale` per window), a verbatim stream's images by reference.
    pub dyn_scale: LaneBits,
    /// D3038 (A): the code lanes the site reads only through `OCTET_LENGTH` tables and the emptiness test (exec sites.rs
    /// `lens_only`, `table_fn::FLAG_SOLE`): a verbatim part that sealed the column's octet-length lane codes such a lane by
    /// length from that lane (kept.rs `open_lens`, text_hosts `lane_codes`), never from its text.
    pub lens_only: LaneBits,
    /// The shredded jsonb path lanes (`with_shred_lane`): path `path` of jsonb column `attnos[lane]`, resolved to its `path_ord` at open.
    pub shred: Arc<[ShredLane]>,
    /// M6 D2 (`with_late_guard`): `(column lane, path lane)`: a late jsonb column lane is filled, before the program reads
    /// the window, where one of its path lanes is invalid (the rows the program walks), and holds the empty image elsewhere
    /// until a later fill asks for those rows.
    pub late_guards: Arc<[(u8, u8)]>,
    /// Per lane, the metadata profile its bloom constant is lowered through (H02b); `None`: no bloom verdict on it.
    pub profiles: Arc<[Option<MetaProfile>]>,
    /// The site's host verdicts (`with_verdict_cell` gives each execution its own).
    pub verdicts: Arc<VerdictCell>,
    /// The site's condition-cache verdict lane (`LaneId::Verdict(0)`), when it declares one.
    pub verdict_lane: Option<usize>,
    /// The relation is permanent: it enters the condition cache when the execution takes the cache (`VERDICT_COND`).
    pub cond_ok: bool,
    /// The relation is UNLOGGED: it enters the condition cache when the execution takes the cache, which
    /// exec's src.verdicts grants an unlogged relation only under `pgrust.condition_cache_unlogged` (default on). A
    /// temporary relation never enters (neither flag).
    pub cond_unlogged: bool,
    /// TABLESAMPLE (`with_sample`): the method (BERNOULLI / SYSTEM, whose picks are a pure function of the position) and
    /// whether a REPEATABLE word follows its argument; the site's key cells 2 and 3 hold them (0 and 1 are the claim).
    pub sample: Option<(tablesample::Tsm, bool)>,
    /// D1536 (`with_skipped_rows`, an EXPLAIN ANALYZE execution's recipe only): the rows its sources' zone verdicts skipped
    /// (a part, a granule), deleted rows left out. PostgreSQL's scan reads each of them and its qual removes it, so entry
    /// instrument.rs adds the count to the scan's Rows Removed by Filter. With it the sources select no row inside a
    /// granule (row_select, a PSMA window): every row of a staged window reaches the program, which counts what its qual
    /// removes. Decided once per execution at bind, never per granule.
    pub skipped: Option<Arc<AtomicU64>>,
    /// PS1-05: the site's last `zone_keys` key cells are open-time zone comparands (`SiteKind::RangeSource::zkeys`), never
    /// one of the source's own cells (the claim, the tid range, the sample's).
    pub zone_keys: u16,
    /// SCANSTATS: the lane of the relation's leading `sorted_by` key, when the site reads it (recorded at plan time: a
    /// bind tells a key-filtered scan by lane, without opening the relation).
    pub key_lane: Option<u16>,
    registry: Arc<PartRegistry>,
}

/// A shredded jsonb path as a lane: `kind` as sealed (`pgrc_format::shredlane`); a number path's mantissa is int8 at scale 0, else a fixed lane at `scale`.
#[derive(Clone, Debug)]
pub struct ShredLane {
    pub lane: u8,
    pub path: Arc<str>,
    pub kind: ShredLaneKind,
    pub scale: i32,
    /// jsonops (3): the path's presence as a byval word (`with_shred_presence_lane`), not its values.
    pub presence: bool,
    /// D3038: the column's own payload byte length per row as a byval word (`with_octet_lane`); its path is empty.
    pub octet: bool,
    /// D3038 (R2): with `octet`, the row's emptiness instead (`with_nonempty_lane`): 1 where the length is not 0.
    pub nonempty: bool,
    /// D3038 (R3): with `octet`, the row's character length instead (`with_char_lane`).
    pub chars: bool,
}

impl ShredLane {
    /// `kind` as its program code (`gravity_program::shred_kind`).
    pub const fn code_of(kind: ShredLaneKind) -> u8 {
        use gravity_program::shred_kind as c;
        match kind {
            ShredLaneKind::Text => c::TEXT,
            ShredLaneKind::Uuid16 => c::UUID16,
            ShredLaneKind::NumericFs => c::NUMERIC_FS,
            ShredLaneKind::Bool => c::BOOL,
        }
    }

    /// Program code `code`'s kind (`gravity_program::shred_kind`); `None` for an unknown code.
    pub const fn kind_of(code: u8) -> Option<ShredLaneKind> {
        use gravity_program::shred_kind as c;
        match code {
            c::TEXT => Some(ShredLaneKind::Text),
            c::UUID16 => Some(ShredLaneKind::Uuid16),
            c::NUMERIC_FS => Some(ShredLaneKind::NumericFs),
            c::BOOL => Some(ShredLaneKind::Bool),
            _ => None,
        }
    }

    /// `(kind, width)` per shred kind: text/uuid by reference; a number path int8 at scale 0 else a fixed 8-byte mantissa; bool a fixed byte.
    pub fn lane_of(kind: ShredLaneKind, scale: i32) -> (u8, u8) {
        match kind {
            ShredLaneKind::Text | ShredLaneKind::Uuid16 => (lane::KIND_BYREF, 8),
            ShredLaneKind::NumericFs if scale == 0 => (lane::KIND_BYVAL, 8),
            ShredLaneKind::NumericFs => (lane::KIND_FIXED, 8),
            ShredLaneKind::Bool => (lane::KIND_FIXED, 1),
        }
    }
}

/// The `path_ord` of shred path `path` of column `attno` in `part` at `kind`/`scale`: every PathTable position spelling the path is
/// tried, the one whose Values stream sits at `(attno, ord)`; `None` when not elected, an error when elected at another kind or scale.
pub fn shred_lane_ord(part: &OpenPart, attno: u32, path: &str, kind: ShredLaneKind, scale: i32) -> PgResult<Option<u32>> {
    match shred_lane_elected(part, attno, path, kind, scale)? {
        Elected::At(o) => Ok(Some(o)),
        Elected::No => Ok(None),
        Elected::OtherKind => Err(err("gravity: a shred lane sealed at another kind")),
        Elected::OtherScale => Err(err("gravity: a shred lane sealed at another scale")),
    }
}

/// How a part elected a shred path (`shred_lane_elected`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Elected {
    /// At this `path_ord`, at the kind and scale asked.
    At(u32),
    /// Not elected.
    No,
    /// Elected at another kind.
    OtherKind,
    /// A number path elected at another scale.
    OtherScale,
}

/// `shred_lane_ord`'s answer without the refusal: a scan publishes every answer but `At` as an all-invalid lane.
pub fn shred_lane_elected(part: &OpenPart, attno: u32, path: &str, kind: ShredLaneKind, scale: i32) -> PgResult<Elected> {
    Ok(match shred_path_sealed(part, attno, path)? {
        None => Elected::No,
        Some((_, k, _)) if k != Some(kind) => Elected::OtherKind,
        Some((_, _, s)) if kind == ShredLaneKind::NumericFs && s != Ok(scale) => Elected::OtherScale,
        Some((ord, _, _)) => Elected::At(ord),
    })
}

/// Path `path` of column `attno` as `part` sealed it: its `path_ord`, its lane kind and its number scale; `None` when
/// the part did not elect it.
fn shred_path_sealed(part: &OpenPart, attno: u32, path: &str) -> PgResult<Option<(u32, Option<ShredLaneKind>, core::result::Result<i32, &'static str>)>> {
    let Some(paths) = pgrc_read::read_path_table(part).map_err(read_err)? else { return Ok(None) };
    let dir = part.stream_directory().map_err(read_err)?;
    // bounded("catalog-sized: one part's path table: the jsonb paths it shredded, at most the relation's pgrc_shred_max_paths reloption (64 by default)")
    for (pos, _) in paths.iter().enumerate().filter(|(_, p)| p.as_str() == path) {
        let ord = pos as u32 + 1;
        let Some(v) = dir.lookup(attno, ord, StreamRole::Values) else { continue };
        return Ok(Some((ord, ShredLaneKind::of_entry(v.entry.class, v.entry.fixed_len), numeric_lane_scale(v.entry.flags, v.entry.aux32))));
    }
    Ok(None)
}

/// D3038 (R1): the `path_ord` of column `attno`'s stored octet-length lane in `part` (`OCTET_LENGTH_PATH`, an int4 word
/// a row), `None` where the part sealed none (a dictionary-coded column, a part sealed before the lane).
pub fn octet_lane_ord(part: &OpenPart, attno: u32) -> PgResult<Option<u32>> {
    let Some(paths) = pgrc_read::read_path_table(part).map_err(read_err)? else { return Ok(None) };
    let dir = part.stream_directory().map_err(read_err)?;
    // bounded("catalog-sized: one part's path table: the jsonb paths it shredded, at most the relation's pgrc_shred_max_paths reloption (64 by default)")
    for (pos, _) in paths.iter().enumerate().filter(|(_, p)| p.as_str() == pgrc_format::shredlane::OCTET_LENGTH_PATH) {
        let ord = pos as u32 + 1;
        if dir.lookup(attno, ord, StreamRole::Values).is_some_and(|v| v.entry.class == pgrc_format::class::CLASS_BYVAL) {
            return Ok(Some(ord));
        }
    }
    Ok(None)
}

/// The shred path of the keys `keys` (outermost first) as the parts seal it (`encode_shred_path`: both sides through it).
pub fn shred_path(keys: &[&str]) -> String {
    pgrc_format::shredlane::encode_shred_path(keys.iter().copied())
}

/// The lowering-time read: the `(kind, scale)` a shred lane of path `path` (`encode_shred_path`
/// form) of jsonb column `attno` is compiled at: the first of `list`'s parts (the lowering's resident parts, hosts facts.rs
/// `RelcacheFacts::resident`) that elected it fixes both;
/// every part's path table is asked until one did (no part count cap). `None`: none of them did (the value form). Any
/// part, now or of a later generation, that did not elect it at that kind and scale publishes the lane all-invalid, and
/// every such row walks its image: the answer is never a correctness fact.
pub fn shred_election(list: &PartList, attno: u32, path: &str) -> PgResult<Option<(ShredLaneKind, i32)>> {
    // a tick per part: each reads its path table
    let mut step = cancelpoll::step("granule_source shred_election: the parts' path tables");
    for spec in list.parts() {
        step.tick().map_err(|_| err(WALK_CANCELED))?;
        let Some(part) = spec.part.as_ref() else { continue };
        if let Some((_, Some(kind), scale)) = shred_path_sealed(part, attno, path)? {
            return Ok(match (kind, scale) {
                (ShredLaneKind::NumericFs, Ok(s)) => Some((kind, s)),
                (ShredLaneKind::NumericFs, Err(_)) => None,
                _ => Some((kind, 0)),
            });
        }
    }
    Ok(None)
}

/// jsonops (3), the lowering-time read: whether every part of `list` that elected Text path `path` of jsonb column
/// `attno` sealed its Values entry `STREAMF_SHRED_OBJECT_ROOTS`, and one did. Never a correctness fact
/// (`decode_presence`: an unflagged part's presence lane is all-invalid).
pub fn shred_object_roots(list: &PartList, attno: u32, path: &str) -> PgResult<bool> {
    let mut flagged = false;
    // a tick per part: each reads its path table
    let mut step = cancelpoll::step("granule_source shred_object_roots: the parts' path tables");
    for spec in list.parts() {
        step.tick().map_err(|_| err(WALK_CANCELED))?;
        let Some(part) = spec.part.as_ref() else { continue };
        let Some(paths) = pgrc_read::read_path_table(part).map_err(read_err)? else { continue };
        let dir = part.stream_directory().map_err(read_err)?;
        let ord = paths.iter().position(|p| p.as_str() == path).map(|pos| pos as u32 + 1);
        let Some(v) = ord.and_then(|o| dir.lookup(attno, o, StreamRole::Values)) else { continue };
        if ShredLaneKind::of_entry(v.entry.class, v.entry.fixed_len) != Some(ShredLaneKind::Text) {
            continue;
        }
        if v.entry.flags & pgrc_format::part::STREAMF_SHRED_OBJECT_ROOTS == 0 {
            return Ok(false);
        }
        flagged = true;
    }
    Ok(flagged)
}

/// SRC-30 (`Facts::shred_dict`, the lowering-time read beside `shred_election`): whether every part of `list` that elected
/// text path `path` of jsonb column `attno` sealed its values as the writer's executable dictionary (`DictCodes` with
/// `DICT_EXEC` and no Validity stream, or the nullable twin: `dict`'s test for the writer's codes), so its code lane opens
/// that dictionary; a part that stored the path verbatim publishes byte ranks it computes at each open (`Dict::Ranked`),
/// about an entry a value. False when no part elected it. Never a correctness fact.
pub fn shred_dict(list: &PartList, attno: u32, path: &str) -> PgResult<bool> {
    use pgrc_format::part::{STREAMF_DICT_EXEC, STREAMF_DICT_EXEC_NULLABLE};
    let mut sealed = false;
    // a tick per part: each reads its path table
    let mut step = cancelpoll::step("granule_source shred_dict: the parts' path tables");
    for spec in list.parts() {
        step.tick().map_err(|_| err(WALK_CANCELED))?;
        let Some(part) = spec.part.as_ref() else { continue };
        let Some(paths) = pgrc_read::read_path_table(part).map_err(read_err)? else { continue };
        let dir = part.stream_directory().map_err(read_err)?;
        let ord = paths.iter().position(|p| p.as_str() == path).map(|pos| pos as u32 + 1);
        let Some((ord, v)) = ord.and_then(|o| dir.lookup(attno, o, StreamRole::Values).map(|v| (o, v))) else { continue };
        if ShredLaneKind::of_entry(v.entry.class, v.entry.fixed_len) != Some(ShredLaneKind::Text) {
            continue;
        }
        let f = v.entry.flags;
        let exec = f & STREAMF_DICT_EXEC_NULLABLE != 0 || (f & STREAMF_DICT_EXEC != 0 && dir.lookup(attno, ord, StreamRole::Validity).is_none());
        if v.entry.encoding != EncodingId::DictCodes as u16 || !exec {
            return Ok(false);
        }
        sealed = true;
    }
    Ok(sealed)
}

fn err(msg: &'static str) -> Box<PgError> {
    crate::pg_err(msg)
}

/// `(kind, width)` per storage class: a by-value word `KIND_BYVAL` at its width (sign-extended per the `SIGNED` flag), float/bool
/// `KIND_FIXED` at its width, a by-reference image `KIND_BYREF` width 8 (a datum pointer into the lane's arena or the dictionary payload).
pub fn lane_of_class(c: StorageClass) -> (u8, u8) {
    match c {
        StorageClass::ByvalWord { width, .. } => (lane::KIND_BYVAL, width),
        StorageClass::F32 => (lane::KIND_FIXED, 4),
        StorageClass::F64 => (lane::KIND_FIXED, 8),
        StorageClass::Bool => (lane::KIND_FIXED, 1),
        StorageClass::Fixed { .. } | StorageClass::VarlenaVerbatim => (lane::KIND_BYREF, 8),
    }
}

/// The extent-image bytes an in-place lane's body must have after it (`GranuleRangeSource::in_place`): a consumer's
/// widest vector load (a 64-byte NEON / AVX-512 register) starting at the last row stays inside the image.
const IN_PLACE_SLACK: usize = 64;

/// A by-reference lane's arena at bind; it doubles while a granule's images do not fit, the data's size its only bound
/// (D2582: a 1 GiB ceiling refused a granule of 140 KB values where PostgreSQL returns the rows), each growth reserved
/// fallibly, so a machine that cannot hold the granule is the decode's typed error, never an abort.
const ARENA_FIRST: usize = 64 << 10;

/// Grow `arena` to `n` bytes (zeroed), fallibly: false when the machine cannot hold them.
fn grow_arena(arena: &mut Vec<u8>, n: usize) -> bool {
    if n > arena.len() && arena.try_reserve_exact(n - arena.len()).is_err() {
        return false;
    }
    arena.resize(n.max(arena.len()), 0); // alloc: Cold
    true
}

/// `grow_arena` to twice its length.
fn double_arena(arena: &mut Vec<u8>) -> bool {
    let n = arena.len() * 2;
    grow_arena(arena, n)
}

fn read_err(e: pgrc_read::ReadError) -> Box<PgError> {
    pgrc_am::read_error(e)
}

fn table_err(e: crate::dict::text_hosts::TableBuildError) -> Box<PgError> {
    match e {
        crate::dict::text_hosts::TableBuildError::Region(gravity_dict::BuildError::TooManyCodes) => err("gravity: a dictionary past the table's code range"),
        crate::dict::text_hosts::TableBuildError::Region(gravity_dict::BuildError::NoSeed) => err("gravity: the fingerprint seed is not installed"),
        crate::dict::text_hosts::TableBuildError::Region(_) => err("gravity: table build"),
        crate::dict::text_hosts::TableBuildError::Pg(e) => e,
        crate::dict::text_hosts::TableBuildError::Read(e) => read_err(e),
    }
}

impl GranuleRecipe {
    /// D4631: the heap blocks a copy of this recipe owns alone (its per-column lists and lane sets; the `Arc` fields are
    /// shared with the copy it came from): a source's own copy (`RecipeHome::Own`, a clone at each slot's build) counts
    /// them in its kept bytes, so they move with its charge.
    pub(crate) fn heap_bytes(&self) -> usize {
        use gravity_driver::vec_bytes as v;
        v(&self.attnos) + v(&self.widths) + v(&self.kinds) + v(&self.scales) + self.numeric.heap_bytes() + self.dyn_scale.heap_bytes() + self.lens_only.heap_bytes()
    }

    /// A granule scan of `rel` projecting `attnos`: every column a lane by its storage class (`lane_of_class`; code lanes via `from_plan_codes`).
    pub fn from_plan(rel: &Relation<'_>, attnos: &[i16]) -> PgResult<GranuleRecipe> {
        GranuleRecipe::from_plan_codes(rel, attnos, &[])
    }

    /// `from_plan` with the lanes in `codes` (a mask over `attnos`) being text /
    /// varchar columns published as dictionary code lanes (step 4): a part that
    /// dictionary-coded the column publishes its codes, a part that stored it
    /// verbatim publishes the byte-ranked codes of its distinct values (`Dict::Ranked`, cached with its `Fp128` table).
    pub fn from_plan_codes(rel: &Relation<'_>, attnos: &[i16], codes: &[u64]) -> PgResult<GranuleRecipe> {
        GranuleRecipe::from_plan_lanes(rel, attnos, codes, &[])
    }

    /// `from_plan_codes` with the lanes in `packed` (a mask over `attnos`) being unconstrained numeric columns read as
    /// their packed codes (N14, `dyn_scale`): a packed stream's mantissas at its elected scale, a verbatim one's images.
    pub fn from_plan_lanes(rel: &Relation<'_>, attnos: &[i16], codes: &[u64], packed: &[u64]) -> PgResult<GranuleRecipe> {
        // D2989: a pgrcolumnar materialized view is the same storage as a pgrcolumnar table
        if !matches!(rel.rd_rel.relkind, RELKIND_RELATION | RELKIND_MATVIEW) || TableAm::of(rel) != Some(TableAm::Pgrcolumnar) {
            return Err(err("gravity: pgrc AM only"));
        }
        if rel.rd_islocaltemp || attnos.is_empty() {
            return Err(err("gravity: granule source shape"));
        }
        let schemas = pgrc_am::schema::col_schemas(rel)?;
        let mut widths = vec![8u8; attnos.len()]; // alloc: Program
        let mut kinds = vec![lane::KIND_BYVAL; attnos.len()]; // alloc: Program
        let mut scales = vec![0u32; attnos.len()]; // alloc: Program
        let mut numeric = LaneBits::new(attnos.len());
        let mut dyn_scale = LaneBits::new(attnos.len());
        // bounded("key-columns: one lane per scanned column, found among the relation's columns at plan time, at most MaxTupleAttributeNumber (1664) columns")
        for (k, &a) in attnos.iter().enumerate() {
            let Some(c) = schemas.iter().find(|c| a > 0 && c.attno == a as u32) else {
                return Err(err("gravity: granule source attno"));
            };
            if laneset::has(packed, k) {
                // N14: an unconstrained numeric (no typmod, no domain), whose packed streams each elect their own scale
                if c.semantics != TypeSemantics::NumericUnpacked || rel.rd_att.attrs.get(a as usize - 1).map(|t| t.atttypid) != Some(NUMERICOID) {
                    return Err(err("gravity: a packed numeric lane needs an unconstrained numeric column"));
                }
                (kinds[k], widths[k]) = (lane::KIND_FIXED, 8);
                numeric.set(k);
                dyn_scale.set(k);
                continue;
            }
            if laneset::has(codes, k) {
                // D2700: a domain over text / varchar / bpchar stores its base's bytes (pgrc_am's schema resolves it)
                let typid = rel.rd_att.attrs.get(a as usize - 1).and_then(|t| lsyscache::typ::getBaseType(t.atttypid).ok());
                if c.class != StorageClass::VarlenaVerbatim || !typid.is_some_and(gravity_fn::typesem::pgrc::code_text) {
                    return Err(err("gravity: a code lane needs a text / varchar / bpchar column"));
                }
                widths[k] = 4;
                kinds[k] = lane::KIND_CODES;
                continue;
            }
            let (kind, w) = lane_of_class(c.class);
            kinds[k] = kind;
            widths[k] = w;
            if let (TypeSemantics::PackedNumeric { scale }, Some(NUMERICOID)) = (c.semantics, rel.rd_att.attrs.get(a as usize - 1).map(|t| t.atttypid)) {
                kinds[k] = lane::KIND_FIXED;
                widths[k] = 8;
                scales[k] = scale as u32;
                numeric.set(k);
            }
        }
        let mut r = GranuleRecipe::for_test_widths(rel.rd_id, attnos, &widths, pgrc_am::inval::registry());
        r.profiles = attnos.iter().map(|&a| schemas.iter().find(|c| c.attno == a as u32).map(|c| MetaProfile::derive_or_opaque(c.class, c.collation_class, c.semantics))).collect();
        r.kinds = kinds;
        r.scales = scales;
        r.numeric = numeric;
        r.dyn_scale = dyn_scale;
        r.cond_ok = rel.rd_rel.relpersistence == RELPERSISTENCE_PERMANENT;
        r.cond_unlogged = rel.rd_rel.relpersistence == RELPERSISTENCE_UNLOGGED;
        let lead = pgrc_am::reloptions::sorted_by_lead_attno(rel);
        r.key_lane = lead.and_then(|k| attnos.iter().position(|&a| a > 0 && a as u32 == k)).map(|k| k as u16);
        Ok(r)
    }

    /// Lane `k` as a `numeric(p, scale)` lane without a relation: the packed mantissa in place, a verbatim part's images by reference.
    pub fn with_numeric_lane(mut self, k: usize, scale: u32) -> GranuleRecipe {
        assert!(k < self.natts && self.kinds[k] != lane::KIND_CODES, "a numeric lane over a plain lane");
        self.kinds[k] = lane::KIND_FIXED;
        self.widths[k] = 8;
        self.scales[k] = scale;
        self.numeric.set(k);
        self
    }

    /// The recipe without a relation, lane `k` a dictionary code lane where `codes` (lane-set words) has `k`.
    pub fn for_test_codes(rel: Oid, attnos: &[i16], codes: &[u64], registry: Arc<PartRegistry>) -> GranuleRecipe {
        let widths: Vec<u8> = (0..attnos.len()).map(|k| if laneset::has(codes, k) { 4 } else { 8 }).collect(); // alloc: Cold
        let mut r = GranuleRecipe::for_test_widths(rel, attnos, &widths, registry);
        // bounded("key-columns: one lane per column of a test recipe, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..attnos.len() {
            if laneset::has(codes, k) {
                r.kinds[k] = lane::KIND_CODES;
            }
        }
        r
    }

    /// The recipe with table lanes over its code lanes (`text_hosts::TableSpec`; the site declares them
    /// with `LaneDesc::table(i)` in the same order after the offset lane).
    /// The recipe with code lane `k` published as its parts' dictionary entries.
    pub fn with_entries(mut self, k: usize) -> GranuleRecipe {
        assert!(k < self.natts && self.kinds[k] == lane::KIND_CODES, "the entries lane is a code lane");
        crate::ops::scan::pgrc::entry_runs::install();
        self.entries = Some(k);
        self
    }

    /// The recipe with every lane (each a code lane, at least two) published as its parts' dictionary entries side by
    /// side (PS1-19, `stage_entries`): a part publishes them only when it would publish each lane's alone.
    pub fn with_entries_every(mut self) -> GranuleRecipe {
        assert!(self.natts >= 2 && self.kinds[..self.natts].iter().all(|&k| k == lane::KIND_CODES), "every entries lane is a code lane");
        self.entries = Some(0);
        self.entries_every = true;
        self
    }

    /// The entries window's entries are filtered by the program (PS1-18): no whole-part entry runs.
    pub fn with_entries_filtered(mut self) -> GranuleRecipe {
        self.entries_filtered = true;
        self
    }

    /// D2981: the entries window's declarer is a descending top over the lane: each part publishes its codes from its
    /// highest down (`stage_entries`), so code.cut's early end reads its windows as an ascending top reads the rank order.
    pub fn with_entries_desc(mut self) -> GranuleRecipe {
        self.entries_desc = true;
        self
    }

    /// RD-dict.entries: the entries window's declarer keeps NULL as a value (`SELECT DISTINCT x`, `GROUP BY x` alone): a
    /// part that holds a NULL in the lane publishes one NULL row after its entries, a part without stats records its rows.
    pub fn with_entries_nulls(mut self) -> GranuleRecipe {
        assert!(!self.entries_every, "the every-lane window counts DISTINCT values only");
        self.entries_nulls = true;
        self
    }

    pub fn with_sparse_dicts(mut self) -> GranuleRecipe {
        self.sparse_dicts = true;
        self
    }

    pub fn with_dict_renders(mut self, renders: u64) -> GranuleRecipe {
        self.dict_renders = renders;
        self
    }

    /// D3038 (A): code lane `k` is read only through `OCTET_LENGTH` tables and the emptiness test (`lens_only`).
    pub fn with_lens_only(mut self, k: usize) -> GranuleRecipe {
        assert!(k < self.natts && self.kinds[k] == lane::KIND_CODES, "a length-only lane is a code lane");
        self.lens_only.set(k);
        self
    }

    pub fn with_head_prefault(mut self) -> GranuleRecipe {
        self.head_prefault = true;
        self
    }

    pub fn with_lane_elide(mut self) -> GranuleRecipe {
        self.lane_elide = true;
        self
    }

    /// A fresh verdict cell: one admission's (exec's BIND; the cached template's cell is never shared across admissions,
    /// and an execution clears what it wrote at its end).
    pub fn with_verdict_cell(mut self) -> GranuleRecipe {
        self.verdicts = Arc::new(VerdictCell::new()); // alloc: Exec
        self
    }

    /// D1536: an EXPLAIN ANALYZE execution's recipe (`instrumented`) counts the rows its zone verdicts skip (`skipped`), a
    /// fresh count per execution; any other execution's counts nothing.
    pub fn with_skipped_rows(mut self, instrumented: bool) -> GranuleRecipe {
        self.skipped = instrumented.then(|| Arc::new(AtomicU64::new(0))); // alloc: Exec
        self
    }

    /// The rows this execution's zone verdicts skipped (`with_skipped_rows`), `None` when it counts none.
    pub fn skipped_rows(&self) -> Option<u64> {
        self.skipped.as_ref().map(|c| c.load(Ordering::Relaxed))
    }

    /// TABLESAMPLE over the scan: refused over pgrc. The lowering refuses it (lowerv4 ops/scan/pgrc/lower.rs `make_with`)
    /// and a seek of a sampled recipe errors: pgrc's ctid is the row's stable ID, not the position a positional sampler
    /// picks by. The recipe form stays for the lowering's shape.
    /// `handler` is the plan's `tsmhandler`; a method other than BERNOULLI / SYSTEM is refused (its picks depend on the
    /// scan's progress: SYSTEM_ROWS, SYSTEM_TIME).
    pub fn with_sample(mut self, handler: Oid, repeatable: bool) -> PgResult<GranuleRecipe> {
        match tablesample::Tsm::from_handler(handler) {
            Some(t @ (tablesample::Tsm::Bernoulli | tablesample::Tsm::System)) => {
                self.sample = Some((t, repeatable));
                Ok(self)
            }
            _ => Err(err("gravity: a pgrc TABLESAMPLE method other than BERNOULLI / SYSTEM")),
        }
    }

    /// PS1-05: the site's last `n` key cells are open-time zone comparands (`SiteKind::RangeSource::zkeys`).
    pub fn with_zone_keys(mut self, n: u16) -> GranuleRecipe {
        self.zone_keys = n;
        self
    }

    /// Lane `k` of the site is the condition cache's verdict lane.
    pub fn with_verdict_lane(mut self, k: usize) -> GranuleRecipe {
        self.verdict_lane = Some(k);
        self
    }

    /// Lane `k`'s metadata profile (a test's stand-in for the catalog's).
    pub fn with_profile(mut self, k: usize, p: MetaProfile) -> GranuleRecipe {
        let mut v = self.profiles.to_vec(); // alloc: Cold
        v[k] = Some(p);
        self.profiles = Arc::from(v);
        self
    }

    /// Lane `k` over jsonb column `attnos[k]` as shredded path `path` (`encode_shred_path` form) at `kind`/`scale`: the path's typed
    /// values, NULL where the document lacks it or holds another type (`->>` semantics); a part that did not elect it refuses at open.
    /// Its faces are the path's (M6 Decision 8c): the Stats / bloom / PSMA and the dictionary sealed at its `path_ord`, and its
    /// metadata profile is the lane's class's, so zone verdicts, elision and answers read the path, never its column. A code
    /// lane (`with_shred_code_lane`) stays one: a Text path's codes (R13-L03).
    pub fn with_shred_lane(mut self, k: usize, path: &str, kind: ShredLaneKind, scale: i32) -> GranuleRecipe {
        assert!(k < self.natts, "a shred lane outside the recipe");
        if self.kinds[k] == lane::KIND_CODES {
            assert!(kind == ShredLaneKind::Text, "a shred code lane is a text path");
        } else {
            let (kind_k, width) = ShredLane::lane_of(kind, scale);
            self.kinds[k] = kind_k;
            self.widths[k] = width;
            self.scales[k] = scale as u32;
        }
        let attno = self.attnos[k].max(0) as u32;
        let schema = kind.col_schema(attno);
        let mut profiles: Vec<Option<MetaProfile>> = self.profiles.to_vec(); // alloc: Cold
        profiles.resize(self.natts, None); // alloc: Cold
        profiles[k] = Some(MetaProfile::derive_or_opaque(schema.class, schema.collation_class, schema.semantics));
        self.profiles = Arc::from(profiles);
        let mut v: Vec<ShredLane> = self.shred.iter().filter(|s| s.lane as usize != k).cloned().collect(); // alloc: Cold
        v.push(ShredLane { lane: k as u8, path: Arc::from(path), kind, scale, presence: false, octet: false, nonempty: false, chars: false });
        self.shred = Arc::from(v);
        self
    }

    /// Lane `k` over jsonb column `attnos[k]` as the codes of text path `path` (R13-L03): the dictionary the part sealed at
    /// the path's `path_ord` where its codes are executable (`DICT_EXEC`, or the nullable twin), else the part's byte ranks of
    /// the path's values; a NULL row (the document NULL or without the path) is an invalid row, its code no value.
    pub fn with_shred_code_lane(mut self, k: usize, path: &str) -> GranuleRecipe {
        assert!(k < self.natts, "a shred lane outside the recipe");
        self.kinds[k] = lane::KIND_CODES;
        self.widths[k] = 4;
        self.scales[k] = 0;
        self.with_shred_lane(k, path, ShredLaneKind::Text, 0)
    }

    /// jsonops (3): lane `k` over jsonb column `attnos[k]` as the presence of text path `path` (a top-level key), a byval
    /// word per row: 1 where the path's lane row is valid, 0 where it is not, valid only where the document is not NULL and
    /// only in a part whose path lane is sealed STREAMF_SHRED_OBJECT_ROOTS (every document an object root; with the
    /// exception-freedom seal law an invalid lane row there is a document without the key). Every row of any other part
    /// (the flag or the path absent) is invalid: its key test reads the document.
    pub fn with_shred_presence_lane(self, k: usize, path: &str) -> GranuleRecipe {
        let mut r = self.with_shred_lane(k, path, ShredLaneKind::Text, 0);
        r.kinds[k] = lane::KIND_BYVAL;
        r.widths[k] = 8;
        let v: Vec<ShredLane> = r.shred.iter().map(|s| ShredLane { presence: s.presence || s.lane as usize == k, ..s.clone() }).collect(); // alloc: Cold
        r.shred = Arc::from(v);
        r
    }

    /// D3038: lane `k` over varlena column `attnos[k]` (text, bytea) as its `octet_length`, a byval word per row (lowerv4
    /// strategies/octet_lane.rs): its face is the column's own streams (`Faces::locate`), never its catalog answers,
    /// sidecars or zone records (a shred face's); `decode_octet` fills it, valid where the column is not NULL.
    pub fn with_octet_lane(self, k: usize) -> GranuleRecipe {
        let mut r = self.with_shred_lane(k, "", ShredLaneKind::Text, 0);
        r.kinds[k] = lane::KIND_BYVAL;
        r.widths[k] = 8;
        let v: Vec<ShredLane> = r.shred.iter().map(|s| ShredLane { octet: s.octet || s.lane as usize == k, ..s.clone() }).collect(); // alloc: Cold
        r.shred = Arc::from(v);
        r
    }

    /// D3038 (R2): lane `k` over varlena column `attnos[k]` as its emptiness (`col <> ''`), a byval word per row: an octet
    /// lane (`with_octet_lane`: the column's own streams, valid where the column is not NULL) whose word is 1 where the
    /// length is not 0.
    pub fn with_nonempty_lane(self, k: usize) -> GranuleRecipe {
        let mut r = self.with_octet_lane(k);
        let v: Vec<ShredLane> = r.shred.iter().map(|s| ShredLane { nonempty: s.nonempty || s.lane as usize == k, ..s.clone() }).collect(); // alloc: Cold
        r.shred = Arc::from(v);
        r
    }

    /// D3038 (R3): lane `k` over text column `attnos[k]` as its `length` (characters), a byval word per row: an octet lane
    /// (`with_octet_lane`) whose word is the character count where the column's granule is not all ASCII.
    pub fn with_char_lane(self, k: usize) -> GranuleRecipe {
        let mut r = self.with_octet_lane(k);
        let v: Vec<ShredLane> = r.shred.iter().map(|s| ShredLane { chars: s.chars || s.lane as usize == k, ..s.clone() }).collect(); // alloc: Cold
        r.shred = Arc::from(v);
        r
    }

    /// M6 D2: jsonb column lane `col`, when late, is filled where shred lane `path` (one of its paths) is invalid
    /// (`late_guards`).
    pub fn with_late_guard(mut self, col: usize, path: usize) -> GranuleRecipe {
        assert!(col < self.natts && path < self.natts && self.shred_of(path).is_some() && self.shred_of(col).is_none(), "a late guard's lanes");
        let mut v = self.late_guards.to_vec(); // alloc: Cold
        if !v.contains(&(col as u8, path as u8)) {
            v.push((col as u8, path as u8)); // alloc: Cold
        }
        self.late_guards = Arc::from(v);
        self
    }

    fn shred_of(&self, k: usize) -> Option<&ShredLane> {
        self.shred.iter().find(|s| s.lane as usize == k)
    }

    /// The lanes that are shredded paths, into `out`.
    pub fn shred_lanes(&self, out: &mut LaneBits) {
        out.clear();
        // bounded("key-columns: one shredded path lane per scanned path, at most MaxTupleAttributeNumber (1664) columns")
        for s in self.shred.iter() {
            out.set(s.lane as usize);
        }
    }

    pub fn with_tables(mut self, tables: Vec<crate::dict::text_hosts::TableSpec>) -> GranuleRecipe {
        assert!(tables.iter().all(|t| (t.over as usize) < self.natts && self.kinds[t.over as usize] == lane::KIND_CODES), "a table lane over a code lane");
        self.tables = Arc::from(tables);
        self
    }

    /// Whether a lane is a dictionary code lane (the claims then run whole parts).
    pub fn has_codes(&self) -> bool {
        self.kinds[..self.natts].contains(&lane::KIND_CODES)
    }

    /// The recipe without a relation (width-8 lanes): the part list comes through the cell.
    pub fn for_test(rel: Oid, attnos: &[i16], registry: Arc<PartRegistry>) -> GranuleRecipe {
        GranuleRecipe::for_test_widths(rel, attnos, &vec![8u8; attnos.len()], registry) // alloc: Cold
    }

    /// The recipe without a relation, lane `k` of storage class `classes[k]` (`lane_of_class`).
    pub fn for_test_classes(rel: Oid, attnos: &[i16], classes: &[StorageClass], registry: Arc<PartRegistry>) -> GranuleRecipe {
        let widths: Vec<u8> = classes.iter().map(|&c| lane_of_class(c).1).collect(); // alloc: Cold
        let mut r = GranuleRecipe::for_test_widths(rel, attnos, &widths, registry);
        // bounded("key-columns: one lane per column of a test recipe, at most MaxTupleAttributeNumber (1664) columns")
        for (k, &c) in classes.iter().enumerate() {
            r.kinds[k] = lane_of_class(c).0;
        }
        r
    }

    /// The recipe without a relation, each lane at its physical width (1 / 2 / 4 / 8).
    pub fn for_test_widths(rel: Oid, attnos: &[i16], widths: &[u8], registry: Arc<PartRegistry>) -> GranuleRecipe {
        assert!(widths.len() == attnos.len() && widths.iter().all(|w| matches!(w, 1 | 2 | 4 | 8)), "lane widths");
        let n = attnos.len();
        GranuleRecipe { rel, natts: n, attnos: attnos.to_vec(), widths: widths.to_vec(), kinds: vec![lane::KIND_BYVAL; n], tables: Arc::from(Vec::new()), entries: None, entries_every: false, entries_filtered: false, entries_nulls: false, entries_desc: false, sparse_dicts: false, dict_renders: 0, head_prefault: false, lane_elide: false, scales: vec![0; n], numeric: LaneBits::new(n), dyn_scale: LaneBits::new(n), lens_only: LaneBits::new(n), shred: Arc::from(Vec::new()), late_guards: Arc::from(Vec::new()), profiles: Arc::from(vec![None; n]), verdicts: Arc::new(VerdictCell::new()), verdict_lane: None, cond_ok: true, cond_unlogged: false, sample: None, skipped: None, zone_keys: 0, key_lane: None, registry } // alloc: Program
    }
}

/// D4732 (a): recipe `r`'s code lanes (a bit per lane, the first 64) over a column the relation stores verbatim in some
/// part (codec facts `text`, as `is_text_lane_column` reads them), but a lane coded by length (`lens_only`): such a part's
/// first open ranks the whole column (`text_hosts::row_codes`: one decode and a sort of every row by its bytes) unless its
/// table is resident. 0 without the relation's codec facts.
pub fn verbatim_code_lanes(rel: &Relation<'_>, r: &GranuleRecipe) -> PgResult<u64> {
    let Some(codecs) = pgrc_am::footer::footer_codecs(rel)? else { return Ok(0) };
    let mut lanes = 0u64;
    // bounded("small-fixed: one per lane of the recipe, at most 64")
    for k in 0..r.natts.min(64) {
        let a = r.attnos[k];
        if r.kinds[k] == lane::KIND_CODES && !r.lens_only.has(k) && a >= 1 && codecs.codec.get(a as usize - 1) == Some(&pgrc_am::footer::CODEC_TEXT) {
            lanes |= 1 << k;
        }
    }
    Ok(lanes)
}

/// D4732 (a): whether part `p` of `list` would rank a column of `lanes` (`verbatim_code_lanes`) at its first open: one of
/// them has no resident whole-part ranks table of the part (a part the list does not hold open: yes). A part whose
/// column is dictionary-coded reads as pending too while it has no such table (the codec facts are the relation's).
pub fn ranks_pending(list: &PartList, r: &GranuleRecipe, lanes: u64, p: usize) -> bool {
    if lanes == 0 {
        return false;
    }
    let Some(part) = list.parts().get(p).and_then(|s| s.part.as_ref()) else { return true };
    (0..r.natts.min(64)).filter(|&k| lanes >> k & 1 != 0).any(|k| !crate::dict::text_hosts::ranks_resident(part, stream_key(r.attnos[k] as u32, 0)))
}

/// SCANSTATS: the end of a statement Gravity served: the read counts its granule sources added on this (the leader's)
/// thread land in pg_stat and the tables' read telemetry, once (`pgrc_am::scanstats::gravity_statement_end`).
pub fn scanstats_statement_end() {
    pgrc_am::scanstats::gravity_statement_end();
}

/// The relation's parts under `snapshot`: the resident list when the
/// relation's manifest head is unchanged (no opens), else the AM's resolve.
pub fn resolve(rel: &Relation<'_>, snapshot: Option<&SnapshotData<'_>>) -> PgResult<Arc<PartList>> {
    // FASTADD: parts that lack a column of the relation read it from stand-ins (a derived list)
    let list = fastadd::reader_list(rel, crate::ops::scan::pgrc::residency::resident_parts(rel, snapshot)?)?;
    let _ = list_zones(rel, &list);
    Ok(list)
}

/// The zone columns the serial census reads, set on the list once; their ranges expand lazily, only for a census's
/// predicates (the walk skips a column whose map does not cover every granule). D1135: they are the zone facts of the
/// catalog generation whose parts ARE the list's (`list_generation`), that Generation object's own
/// (`generation_zones`), never the session footer facts keyed by the generation number (pgrc's D1128: a same-transaction
/// TRUNCATE restarts the number and a rolled-back publish reuses it, and another generation's ranges set here reached
/// every later session). A list not the latest committed generation's, or a catalog error, leaves them unset (a seek
/// opens every part), asked again at the next resolve.
fn list_zones<'a>(rel: &Relation<'_>, list: &'a PartList) -> Option<&'a [(u32, Arc<ZoneColumn>)]> {
    if list.zones.get().is_none() {
        if let Some((cols, own)) = list_generation(rel, list).ok().flatten().and_then(|g| generation_zones(&g)) {
            list.set_zones(cols, own);
        }
    }
    list.zones.get().map(|z| &z[..])
}

/// `g`'s own zone columns: the facts the Generation object serves (the prime's, a statement's fold, the plan rig's
/// publish), else its facts image under the prime's rule (`serve_generation_without_catalog`, not installed); `None`
/// without an image or when that rule needs the catalog (the census then excludes nothing). `true`: the columns are
/// the caller's alone (not installed).
fn generation_zones(g: &pgrc_am::catalog::Generation) -> Option<(Vec<pgrc_am::footer::ZoneFacts>, bool)> {
    match g.facts(|_| Err(Box::new(PgError::error("gravity: zone facts: not served")))) { // alloc: Cold
        Ok(Some(f)) => Some((f.zones.clone(), false)), // alloc: Cold charged the part cache (PartList::zones_charge: the list's slice)
        Ok(None) => None,
        Err(_) => pgrc_am::footer::serve_generation_without_catalog(g).ok().flatten().map(|f| (f.zones, true)),
    }
}

fn zone_of(zones: &[(u32, Arc<ZoneColumn>)], attno: u32) -> Option<Arc<ZoneColumn>> {
    zones.iter().find(|(z, _)| *z == attno).map(|(_, c)| Arc::clone(c))
}

/// `attno`'s zone column of the relation's latest committed generation (committed or this transaction's), from its
/// resident list (`list_zones`); a relation without a generation keeps the footer's (the plan rig's override facts,
/// keyed by no generation number).
fn visible_zone_column(rel: &Relation<'_>, attno: u32) -> PgResult<Option<Arc<ZoneColumn>>> {
    let list = resolve(rel, None)?;
    if let Some(z) = list_zones(rel, &list) {
        return Ok(zone_of(z, attno));
    }
    match visible(rel)? {
        pgrc_am::catalog::Lookup::Empty => pgrc_am::footer::footer_zone_column(rel, attno),
        _ => Ok(None),
    }
}

/// Registry opens so far in this process (each `open_pinned` is one `open(2)`): the census' `opens=`.
pub fn registry_opens() -> u64 {
    let c = pgrc_am::inval::registry().counters();
    c.hits + c.misses
}

static DECODED: AtomicU64 = AtomicU64::new(0);
static SKIPPED: AtomicU64 = AtomicU64::new(0);
static ANSWERED: AtomicU64 = AtomicU64::new(0);
static PARTS_ANSWERED: AtomicU64 = AtomicU64::new(0);
/// [BUILD LAW] Why a Stats section is read on the served path: `stats_body` takes one, the census counts per reason.
#[derive(Clone, Copy)]
pub enum FirstTouch {
    /// the part goes granule by granule: its granule records are the zone walk's verdicts / order key
    GranuleWalk = 0,
    /// the list carries no catalog answers (the tests' in-memory parts): the plain path
    NoAnswers = 1,
    /// FASTADD: a column the part lacks: its stand-in's Stats (in memory) is the column's answer, no part-file read
    StandIn = 2,
}
static STATS_SECTION_READS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
/// The widest gap `prefault_meta` reads through between two meta sections of a part (the columns' other sections between).
const META_PREFAULT_GAP: u64 = 16 << 10;
static ANSWERS_SERVED: AtomicU64 = AtomicU64::new(0);

static SELECTED: AtomicU64 = AtomicU64::new(0);
/// Lane-granules left undecoded by their own `AllMatch` verdict inside a `Mixed` granule (`lane_elide`).
static LANE_ELIDED: AtomicU64 = AtomicU64::new(0);
static BLOOM_SKIPPED: AtomicU64 = AtomicU64::new(0);
static PSMA_SKIPPED: AtomicU64 = AtomicU64::new(0);
static PSMA_NARROWED: AtomicU64 = AtomicU64::new(0);
static COND_SKIPPED: AtomicU64 = AtomicU64::new(0);
static RT_SKIPPED: AtomicU64 = AtomicU64::new(0);
static PACKED: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static UNREAD: AtomicU64 = AtomicU64::new(0);
static CONST_KEY: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static IN_PLACE: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// V4R-19 (A) so far in this process: `[windows whose declared key lane is constant, windows asked]`.
pub fn const_key_census() -> [u64; 2] {
    [CONST_KEY[1].load(Ordering::Relaxed), CONST_KEY[0].load(Ordering::Relaxed) + CONST_KEY[1].load(Ordering::Relaxed)]
}

/// PGRC-DECODE lever 2 so far in this process: `[lane-granules published in place, by-value lane-granules decoded whole
/// into their slot]` (`GranuleRangeSource::in_place`).
pub fn in_place_census() -> [u64; 2] {
    [IN_PLACE[1].load(Ordering::Relaxed), IN_PLACE[0].load(Ordering::Relaxed)]
}

/// decode.unread so far in this process: the condition-cache hits published as a count, no lane decoded.
pub fn unread_census() -> u64 {
    UNREAD.load(Ordering::Relaxed)
}

/// decode.packed so far in this process: `[windows packed, windows with candidates left unpacked]` (a selection, deletes,
/// or a candidate the block path cannot read).
pub fn packed_census() -> [u64; 2] {
    [PACKED[0].load(Ordering::Relaxed), PACKED[1].load(Ordering::Relaxed)]
}

/// The v4 host verdicts so far in this process: `[granules skipped by a bloom, skipped by an empty PSMA window, narrowed
/// to a PSMA window, skipped by the condition cache, skipped by a join's runtime filter]`.
pub fn verdict_census() -> [u64; 5] {
    [BLOOM_SKIPPED.load(Ordering::Relaxed), PSMA_SKIPPED.load(Ordering::Relaxed), PSMA_NARROWED.load(Ordering::Relaxed), COND_SKIPPED.load(Ordering::Relaxed), RT_SKIPPED.load(Ordering::Relaxed)]
}

/// One source's census counts, flushed into the process statics at a claim's end, at seek and at unbind (before the
/// statement's census reads them: the protocols' state drops after the run closes): a fetch_add per granule put every
/// worker on the statics' cache line (a tenth of a warm parallel scan's time, in window()).
#[derive(Default)]
struct Tally {
    decoded: u64,
    skipped: u64,
    answered: u64,
    parts_answered: u64,
    selected: u64,
    elided: u64,
    bloom: u64,
    psma_skipped: u64,
    psma_narrowed: u64,
    cond_skipped: u64,
    rt_skipped: u64,
    /// `[packed, unpacked]` windows.
    packed: [u64; 2],
    /// Condition-cache lookups `[hits, misses]`.
    cond: [u64; 2],
    unread: u64,
    /// V4R-19 (A)'s windows `[not constant, constant]`.
    const_key: [u64; 2],
    /// PGRC-DECODE lever 2's by-value whole-granule lanes `[copied, in place]`.
    in_place: [u64; 2],
}

impl Tally {
    fn flush(&mut self) {
        let t = core::mem::take(self);
        let rows: [(&AtomicU64, u64); 18] = [
            (&DECODED, t.decoded),
            (&SKIPPED, t.skipped),
            (&ANSWERED, t.answered),
            (&PARTS_ANSWERED, t.parts_answered),
            (&SELECTED, t.selected),
            (&LANE_ELIDED, t.elided),
            (&BLOOM_SKIPPED, t.bloom),
            (&PSMA_SKIPPED, t.psma_skipped),
            (&PSMA_NARROWED, t.psma_narrowed),
            (&COND_SKIPPED, t.cond_skipped),
            (&RT_SKIPPED, t.rt_skipped),
            (&PACKED[0], t.packed[0]),
            (&PACKED[1], t.packed[1]),
            (&UNREAD, t.unread),
            (&CONST_KEY[0], t.const_key[0]),
            (&CONST_KEY[1], t.const_key[1]),
            (&IN_PLACE[0], t.in_place[0]),
            (&IN_PLACE[1], t.in_place[1]),
        ];
        // bounded("small-fixed: the tally's 18 census counters")
        for (a, n) in rows.into_iter().filter(|r| r.1 != 0) {
            a.fetch_add(n, Ordering::Relaxed);
        }
        crate::ops::scan::pgrc::cond_cache::count_n(t.cond[0], t.cond[1]);
    }
}

/// The runtime filter's measuring window (checks); the share of cuts it keeps checking at is the cost home's `rt_div` on
/// this host (`rt_count`).
const RT_PROBE: u32 = gravity_program::cost::pgrc::RT_PROBE;

/// Lane-granules elided by the per-lane verdict so far in this process (the served line's `elided=`).
pub fn lane_elided_census() -> u64 {
    LANE_ELIDED.load(Ordering::Relaxed)
}

/// Granules decoded, skipped on a `NoneMatch` verdict, answered from statistics, the parts
/// answered whole, and the granules decoded selectively: the census' `granules=d/s/a/sel` deltas.
pub fn granule_census() -> [u64; 5] {
    [DECODED.load(Ordering::Relaxed), SKIPPED.load(Ordering::Relaxed), ANSWERED.load(Ordering::Relaxed), PARTS_ANSWERED.load(Ordering::Relaxed), SELECTED.load(Ordering::Relaxed)]
}

/// Stats bodies read from part files per declared reason (`[granule-walk, no-answers]`) and
/// served from the catalog's answers, so far in this process: the census' `first_touch=` / `why=`.
pub fn first_touch_census() -> [u64; 3] {
    [STATS_SECTION_READS[0].load(Ordering::Relaxed), STATS_SECTION_READS[1].load(Ordering::Relaxed), ANSWERS_SERVED.load(Ordering::Relaxed)]
}

static PF_RUNS: AtomicU64 = AtomicU64::new(0);
static PF_BYTES: AtomicU64 = AtomicU64::new(0);
static PF_EXTENTS: AtomicU64 = AtomicU64::new(0);
static PF_RESIDENT: AtomicU64 = AtomicU64::new(0);
static PF_INSERTED_BYTES: AtomicU64 = AtomicU64::new(0);

/// D4709 (debug builds): the read/advice stat RMWs made on the process totals so far (the witness counts adds, not values).
#[cfg(debug_assertions)]
static STAT_RMWS: AtomicU64 = AtomicU64::new(0);

/// D4709: `n` more RMWs on the process-wide stat totals (debug builds only: release builds count nothing).
#[inline(always)]
fn note_rmws(n: u64) {
    #[cfg(debug_assertions)]
    STAT_RMWS.fetch_add(n, Ordering::Relaxed);
    #[cfg(not(debug_assertions))]
    let _ = n;
}

/// D4709 witness: the stat RMWs made in this process so far (0 in a release build).
#[doc(hidden)]
pub fn stat_rmws() -> u64 {
    #[cfg(debug_assertions)]
    return STAT_RMWS.load(Ordering::Relaxed);
    #[cfg(not(debug_assertions))]
    0
}

/// D4709: one call's read counts `[extents inserted, extent bytes inserted, already resident, bytes, preads]` into the
/// process totals: a relaxed add per nonzero count (a hot call, everything resident, adds only what it counted).
fn add_prefetched(pf: [u64; 5]) -> usize {
    let mut rmws = 0;
    // one add per nonzero count, the five totals spelled out (a fixed five, not a loop over items)
    if pf[0] > 0 {
        PF_EXTENTS.fetch_add(pf[0], Ordering::Relaxed);
        rmws += 1;
    }
    if pf[1] > 0 {
        PF_INSERTED_BYTES.fetch_add(pf[1], Ordering::Relaxed);
        rmws += 1;
    }
    if pf[2] > 0 {
        PF_RESIDENT.fetch_add(pf[2], Ordering::Relaxed);
        rmws += 1;
    }
    if pf[3] > 0 {
        PF_BYTES.fetch_add(pf[3], Ordering::Relaxed);
        rmws += 1;
    }
    if pf[4] > 0 {
        PF_RUNS.fetch_add(pf[4], Ordering::Relaxed);
        rmws += 1;
    }
    note_rmws(rmws as u64);
    rmws
}

/// D4709: one claim's WILLNEED calls and bytes into the process totals.
fn add_advised(calls: u64, bytes: u64) -> usize {
    let mut rmws = 0;
    if calls > 0 {
        ADVISE_CALLS.fetch_add(calls, Ordering::Relaxed);
        rmws += 1;
    }
    if bytes > 0 {
        ADVISE_BYTES.fetch_add(bytes, Ordering::Relaxed);
        rmws += 1;
    }
    note_rmws(rmws as u64);
    rmws
}

/// The claim run reads so far in this process: `[preads, bytes, extents inserted, already resident, extent bytes inserted]`.
pub fn read_census() -> [u64; 5] {
    [PF_RUNS.load(Ordering::Relaxed), PF_BYTES.load(Ordering::Relaxed), PF_EXTENTS.load(Ordering::Relaxed), PF_RESIDENT.load(Ordering::Relaxed), PF_INSERTED_BYTES.load(Ordering::Relaxed)]
}

/// The part files' I/O so far in this process (the served line's `io=`): `[preads, bytes, ns inside preads]`, then `read_census()`.
pub fn io_census() -> [u64; 8] {
    let (calls, bytes) = pgrc_read::io::pread_census();
    let r = read_census();
    [calls, bytes, pgrc_read::io::PREAD_NS.load(Ordering::Relaxed), r[0], r[1], r[2], r[3], r[4]]
}

/// The dictionaries' faults so far in this process (the served line's `dict=`): `[whole-section bytes, block faults, block bytes (compressed), frame faults, frame bytes]`.
pub fn dict_census() -> [u64; 5] {
    let (fb, ff, wb) = pgrc_read::dicthandle::dict_fault_census();
    let (bb, bf) = pgrc_read::dicthandle::dict_block_census();
    [wb, bf, bb, ff, fb]
}

/// The dictionary entry accesses so far in this process (the served line's `dictent=`, under
/// `PGRUST_PGRC_DICT_CENSUS=1`): `[random entry() calls, fc entries materialised, cursor entries
/// walked, executor rows gathered (fc), executor entries decoded (fc), renders through dict_value]`.
pub fn entry_census() -> [u64; 6] {
    let (e, m, w, _) = pgrc_read::dicthandle::dict_entry_census();
    [e, m, w, pgrc_codec::dictcodes::FC_GATHER_ROWS.load(Ordering::Relaxed), pgrc_codec::dictcodes::FC_GATHER_DECODES.load(Ordering::Relaxed), DICT_RENDERS.load(Ordering::Relaxed)]
}

static DICT_RENDERS: AtomicU64 = AtomicU64::new(0);

/// The high-water mark of part-file preads in flight since the previous call (reset on each call).
pub fn take_io_depth() -> u64 {
    pgrc_read::io::take_read_depth_max()
}

/// `(max run, max hole)` bytes of a claim's run reads, read once: `pgrust.gravity_read_run_kb` /
/// `PGRUST_GRAVITY_READ_RUN_KB` (0 = per-extent demand reads), `..._hole_kb` / `..._HOLE_KB`.
fn read_tuning() -> (u64, u64) {
    let (run, hole, ..) = read_knobs();
    (run, hole)
}

/// The read knobs, read once together (one once site): `read_tuning`'s run and hole, and read.advise's piece and
/// in-flight budget (`advise_piece`, `advise_budget`).
fn read_knobs() -> (u64, u64, u64, u64) {
    static T: OnceLock<(u64, u64, u64, u64)> = OnceLock::new();
    *T.get_or_init(|| {
        use gravity_program::cost::pgrc::{read_run_bytes, READ_HOLE_BYTES};
        // PostgreSQL's read stream on the session's settings (a process without them takes their boot values)
        let (blocks, ahead) = crate::inputs::read_stream_sizes();
        let (combine, ahead) = (blocks.max(1) as u64 * types_core::BLCKSZ as u64, ahead.max(1) as u64);
        let run = knob_kb("pgrust.gravity_read_run_kb", "PGRUST_GRAVITY_READ_RUN_KB", read_run_bytes(combine, ahead) >> 10);
        let hole = knob_kb("pgrust.gravity_read_hole_kb", "PGRUST_GRAVITY_READ_HOLE_KB", READ_HOLE_BYTES >> 10);
        let (max_io, queue) = device_io();
        let (piece, budget) = advise_knobs(max_io, queue);
        (run, hole, piece, budget)
    })
}

/// The data directory's block device as the claim's advice sizes against it, read once inside `read_knobs`: its largest
/// request in bytes (`max_sectors_kb`) and the requests its queue holds (`nr_requests`), from sysfs; a device sysfs does
/// not describe takes PostgreSQL's own sizes, its combined read (io_combine_limit) and the I/Os it keeps in flight
/// (effective_io_concurrency), as heap's read stream does (heap_window.rs `advice_piece` / `derive`).
fn device_io() -> (u64, u64) {
    let attr = |name: &str| crate::ops::scan::heap_window::device_attr(name).map(u64::from).filter(|&v| v > 0);
    let max_io = attr("max_sectors_kb").map_or_else(|| crate::inputs::io_combine_blocks().max(1) as u64 * types_core::BLCKSZ as u64, |kb| kb << 10);
    let queue = attr("nr_requests").unwrap_or_else(|| crate::inputs::effective_io_concurrency().max(1) as u64);
    (max_io, queue)
}

/// WILLNEED calls / bytes the claims' advice issued (`read_claim_run`, read.advise).
pub static ADVISE_CALLS: AtomicU64 = AtomicU64::new(0);
pub static ADVISE_BYTES: AtomicU64 = AtomicU64::new(0);

/// Bytes advised by claims whose lane reads have not run yet, process-wide (`read_claim_run`, read.advise's budget).
static ADVISE_INFLIGHT: AtomicU64 = AtomicU64::new(0);

/// read.advise's in-flight budget in bytes (read once, `read_knobs`): `pgrust.gravity_read_advise_budget_kb` /
/// `PGRUST_GRAVITY_READ_ADVISE_BUDGET_KB` (0 = unbounded), else the data directory's device queue: `nr_requests` calls of
/// `piece` bytes (the requests the device holds at once), effective_io_concurrency calls when sysfs does not name it.
fn advise_budget() -> u64 {
    let b = read_knobs().3;
    if b == 0 { u64::MAX } else { b }
}

/// Bytes per WILLNEED call of a claim's advice (read once, `read_knobs`), `None` = off: `pgrust.gravity_read_advise_kb` /
/// `PGRUST_GRAVITY_READ_ADVISE_KB` (0 = off), else derived from the data directory's block device — Linux caps one WILLNEED
/// call at the larger of its `max_sectors_kb` and `read_ahead_kb` (`force_page_cache_ra`) — PostgreSQL's combined read
/// (`device_io`) when sysfs has neither.
fn advise_piece() -> Option<u64> {
    let p = read_knobs().2;
    (p > 0).then_some(p)
}

/// read.advise's `(piece, budget)` bytes, computed inside `read_knobs`' once: the sysfs probe of the data directory's device
/// (`device_attr`), the device's request size and queue (`device_io`, which fall back to PostgreSQL's) and the two knobs.
fn advise_knobs(max_io: u64, queue: u64) -> (u64, u64) {
    let derived = ["max_sectors_kb", "read_ahead_kb"].into_iter().filter_map(crate::ops::scan::heap_window::device_attr).max().map(u64::from).filter(|&kb| kb > 0).unwrap_or(max_io >> 10);
    let piece = knob_kb("pgrust.gravity_read_advise_kb", "PGRUST_GRAVITY_READ_ADVISE_KB", derived);
    let budget = if piece == 0 { 0 } else { knob_kb("pgrust.gravity_read_advise_budget_kb", "PGRUST_GRAVITY_READ_ADVISE_BUDGET_KB", queue * piece >> 10) };
    (piece, budget)
}

/// V4R-33: parts presented as one window of their condition-cache survivor total (`whole_part_hit`).
static COND_PARTS: AtomicU64 = AtomicU64::new(0);

/// V4R-33's census: the parts a decode.unread statement counted whole from their condition-cache survivor totals.
pub fn cond_parts_census() -> u64 {
    COND_PARTS.load(Ordering::Relaxed)
}

/// The Stats section body of `attno` (granule records first, `STATS_RECORD_LEN` each); `None` when the part carries none.
/// The key the part-resident tables of stream `(attno, path_ord)` (ranks, fingerprints, condition tables, entry runs) are
/// cached under in `text_hosts`: a column's attno for its own stream (every key as before), `attno | path_ord << 16` for a
/// shredded path lane, never a column's (attno < `MAX_FACE_ATTNO` < 1 << 16).
fn stream_key(attno: u32, ord: u32) -> u32 {
    attno | ord << 16
}

fn stats_body(part: &OpenPart, attno: u32, ord: u32, why: FirstTouch) -> PgResult<Option<Box<[u8]>>> {
    let Some(idx) = part.find_section(SectionKind::Stats, attno, ord) else { return Ok(None) };
    match why {
        FirstTouch::StandIn => ANSWERS_SERVED.fetch_add(1, Ordering::Relaxed),
        _ => STATS_SECTION_READS[why as usize].fetch_add(1, Ordering::Relaxed),
    };
    let wrapped = part.sections()[idx].flags & SECTIONF_META_ZSTD != 0;
    let raw = part.section_bytes(idx).map_err(read_err)?;
    let body = if wrapped { pgrc_codec::wrapper::meta_unwrap_body(raw.bytes()).map_err(|e| read_err(pgrc_read::ReadError::Format(e)))? } else { raw.bytes().to_vec() }; // alloc: Exec
    Ok(Some(body.into_boxed_slice()))
}

/// Section `kind` of stream `(attno, ord)` in `part`, unwrapped (a bloom / PSMA body); `None` when the part sealed none.
fn section_body(part: &OpenPart, kind: SectionKind, attno: u32, ord: u32) -> PgResult<Option<Box<[u8]>>> {
    let Some(idx) = part.find_section(kind, attno, ord) else { return Ok(None) };
    let wrapped = part.sections()[idx].flags & SECTIONF_META_ZSTD != 0;
    let raw = part.section_bytes(idx).map_err(read_err)?;
    let body = if wrapped { pgrc_codec::wrapper::meta_unwrap_body(raw.bytes()).map_err(|e| read_err(pgrc_read::ReadError::Format(e)))? } else { raw.bytes().to_vec() }; // alloc: Exec
    Ok(Some(body.into_boxed_slice()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PartStep {
    Skipped,
    Answered,
    Granules,
}

const V_MIXED: u8 = lane::VERDICT_MIXED;
/// Every registered predicate (`part_verdicts_of` / `verdicts_of`'s mask when no lane is all-match).
const ALL_PREDS: u64 = u64::MAX;

/// Predicate `i`'s bit in a verdict mask: the predicates past the 64th share the top bit, so a mixed one keeps every
/// lane of those predicates out of all-match (the row kernel decides), never a wrong all-match.
fn pred_bit(i: usize) -> u64 {
    1 << i.min(63)
}

/// The predicates `row_select` pre-filters with, one bit each in its `u64` lane mask; the kernel tests the rest per row.
const ROW_PREDS: usize = u64::BITS as usize;
const V_ALL: u8 = lane::VERDICT_ALL_MATCH;
const V_NONE: u8 = lane::VERDICT_NONE_MATCH;
/// D4554: `part_dec`'s entries besides a verdict: the part's record decides nothing (`part_decides` None), or the part had
/// no record when asked (each granule asks again).
const PD_NOTHING: u8 = 0xFE;
const PD_ASK: u8 = 0xFF;
/// `zlen`: the predicate has no constant length.
const NO_LEN: u32 = u32::MAX;

/// The facts a zone verdict, a PSMA window and a runtime-filter cut read from one Stats record, at their spec §8.1 offsets
/// (`StatsRecord::OFF_*`: min_key, max_key, nonnull, key_kind, sortedness), the two kinds validated as
/// the whole decode validates them: verdicts_of asks one per granule and zone predicate on every execution, where the
/// whole 80-byte decode's fifteen checked reads were the scan's largest host cost (the StatsRecord
/// decode and its wire cursor were a quarter of a zone-selective scan's profile).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ZoneFacts {
    min_key: i64,
    max_key: i64,
    nonnull: u32,
    /// The least non-NULL value's payload bytes: 0 where the column keeps no length stats (a by-value one, a record
    /// without them), so only a nonzero one says something (no non-NULL value is empty).
    byte_len_min: u32,
    /// The greatest non-NULL value's payload bytes: 0 where the column keeps no length stats, so it is read only from a
    /// `computed` record of a column that keeps them (`len_verdict`).
    byte_len_max: u32,
    /// The record carries `STATSF_COMPUTED`: its builder computed every stat the column's profile keeps.
    computed: bool,
    key_kind: u8,
    sorted: u8,
}

/// `StatsRecord`'s wire offsets of `byte_len_max` and `flags`: its `repr(C)` layout is the wire's (pgrc_format's layout
/// test pins the fields it names; `zone_facts_are_the_whole_decodes` these).
const OFF_BYTE_LEN_MAX: usize = core::mem::offset_of!(StatsRecord, byte_len_max);
const OFF_FLAGS: usize = core::mem::offset_of!(StatsRecord, flags);

impl ZoneFacts {
    /// The facts of the record at the start of `b`, read at the format's wire offsets (`StatsRecord::OFF_*`); `None`
    /// where the whole decode fails (a short slice, a bad kind).
    #[inline(always)]
    fn of(b: &[u8]) -> Option<ZoneFacts> {
        let r: &[u8; STATS_RECORD_LEN] = b.get(..STATS_RECORD_LEN)?.try_into().ok()?;
        let word = |o: usize| i64::from_le_bytes([r[o], r[o + 1], r[o + 2], r[o + 3], r[o + 4], r[o + 5], r[o + 6], r[o + 7]]);
        let key_kind = r[StatsRecord::OFF_KEY_KIND];
        KeyKind::from_u8(key_kind).ok()?;
        let sorted = r[StatsRecord::OFF_SORTEDNESS];
        Sortedness::from_u8(sorted).ok()?;
        let n = StatsRecord::OFF_NONNULL;
        Some(ZoneFacts {
            min_key: word(StatsRecord::OFF_MIN_KEY),
            max_key: word(StatsRecord::OFF_MAX_KEY),
            nonnull: u32::from_le_bytes([r[n], r[n + 1], r[n + 2], r[n + 3]]),
            byte_len_min: { let m = StatsRecord::OFF_BYTE_LEN_MIN; u32::from_le_bytes([r[m], r[m + 1], r[m + 2], r[m + 3]]) },
            byte_len_max: u32::from_le_bytes([r[OFF_BYTE_LEN_MAX], r[OFF_BYTE_LEN_MAX + 1], r[OFF_BYTE_LEN_MAX + 2], r[OFF_BYTE_LEN_MAX + 3]]),
            computed: u16::from_le_bytes([r[OFF_FLAGS], r[OFF_FLAGS + 1]]) & pgrc_format::meta::STATSF_COMPUTED != 0,
            key_kind,
            sorted,
        })
    }
}

#[cfg(test)]
mod d4709_stat_fold_tests {
    use super::*;

    /// D4709: a call's read and advice counts reach the process totals in one add per nonzero count, exactly: a lane's counts
    /// summed in locals fold to the totals the per-lane adds made, and a call that read and advised nothing adds nothing.
    /// (The totals are read as deltas; no other test in this binary scans.)
    #[test]
    fn a_calls_counts_fold_once_and_exactly() {
        let before = (read_census(), ADVISE_CALLS.load(Ordering::Relaxed), ADVISE_BYTES.load(Ordering::Relaxed));
        let lanes = [[1u64, 100, 0, 4096, 1], [0, 0, 3, 0, 0], [2, 200, 0, 8192, 2], [0; 5]];
        let mut pf = [0u64; 5];
        for l in lanes {
            for (acc, n) in pf.iter_mut().zip(l) {
                *acc += n;
            }
        }
        assert_eq!(add_prefetched(pf), 5, "one add per nonzero total, not per lane and count (4 lanes x 5 counts = 20 before)");
        assert_eq!(add_advised(7, 9000), 2);
        assert_eq!(add_prefetched([0; 5]), 0, "a hot call, everything resident and nothing advised, adds nothing");
        assert_eq!(add_advised(0, 0), 0);
        let after = (read_census(), ADVISE_CALLS.load(Ordering::Relaxed), ADVISE_BYTES.load(Ordering::Relaxed));
        let d = |a: [u64; 5], b: [u64; 5]| [a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3], a[4] - b[4]];
        assert_eq!(d(after.0, before.0), [3, 12288, 3, 3, 300], "[preads, bytes, extents, resident, extent bytes] sum over the lanes");
        assert_eq!((after.1 - before.1, after.2 - before.2), (7, 9000));
    }
}

#[cfg(test)]
mod d4482_zone_body_tests {
    use super::ZoneBody;

    /// D4482 b: a CodeZones body a list's warm cell keeps is the part cache's from its read: its charge holds the body's
    /// bytes on the part cache's holder, and the body reads through it unchanged.
    #[test]
    fn a_code_zones_body_is_the_part_caches_from_its_read() {
        let z = ZoneBody::new((0..4096u32).map(|i| i as u8).collect()); // alloc: Cold
        assert_eq!((z._held.committed(), z._held.owner()), (4096, memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache))));
        assert_eq!((z.len(), z[4095]), (4096, 255));
    }
}

#[cfg(test)]
mod d4537_zone_columns_tests {
    use super::{PartList, ZoneColumn};
    use pgrc_am::footer::ZoneFacts;
    use std::sync::Arc;

    const PART_CACHE: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache));

    fn column() -> ZoneFacts {
        let ranges: Vec<(i64, i64)> = (0..64i64).map(|i| (i * 10, i * 10 + 9)).collect(); // alloc: Cold
        ZoneFacts::new(1, pgrc_format::zonemap::ZoneMap::encode(&ranges).expect("encodes")).expect("decodes")
    }

    /// D4537 4: a list that owns its zone columns (no generation holds them) charges its slice and the columns' bytes to
    /// the part cache at the set; a list whose columns a generation's served facts hold charges its slice only; a second
    /// set keeps the first columns and their charge.
    #[test]
    fn a_lists_zone_columns_are_the_part_caches_from_the_set() {
        let slot = core::mem::size_of::<(u32, Arc<ZoneColumn>)>() as u64;
        let (own, held) = (PartList::new(Vec::new()), PartList::new(Vec::new())); // alloc: Cold
        let z = column();
        let bytes = z.heap_bytes() as u64;
        assert!(bytes > 64 * 16, "the expanded ranges are the column's");
        own.set_zone_columns(vec![z]); // alloc: Cold
        let c = own.zones_charge.get().expect("charged at the set");
        assert_eq!((c.owner(), c.committed()), (PART_CACHE, slot + bytes));
        held.set_zones(vec![column()], false); // alloc: Cold
        let c = held.zones_charge.get().expect("charged at the set");
        assert_eq!((c.owner(), c.committed()), (PART_CACHE, slot));
        own.set_zone_columns(vec![column(), column()]); // alloc: Cold
        assert_eq!((own.zones.get().map(|z| z.len()), own.zones_charge.get().map(|c| c.committed())), (Some(1), Some(slot + bytes)));
    }
}

#[cfg(test)]
mod zone_facts_tests {
    use super::{KeyKind, Sortedness, StatsRecord, ZoneFacts, STATS_RECORD_LEN};

    /// The offsets are StatsRecord's wire order: every field a zone verdict reads equals the whole decode's, and a record
    /// the decode refuses (a bad key kind or sortedness, a short slice) has no facts.
    #[test]
    fn zone_facts_are_the_whole_decodes() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };
        for i in 0..64u32 {
            let r = StatsRecord { min_key: next() as i64, max_key: next() as i64, sum_i128: next() as i128 * 3, zero_count: next(), byte_len_sum: next(), nonnull: next() as u32, byte_len_min: next() as u32, byte_len_max: next() as u32, char_len_min: 3, char_len_max: 4, ndv_est: 5, key_kind: (i % 4) as u8, sortedness: (i / 4 % 4) as u8, flags: i as u16, pad: 0 };
            let mut b = Vec::new(); // alloc: Cold
            r.encode_into(&mut b);
            b.extend_from_slice(&[0xAB; 7]);
            let f = ZoneFacts::of(&b).expect("a valid record");
            assert_eq!((f.min_key, f.max_key, f.nonnull, f.key_kind, f.sorted, f.byte_len_min), (r.min_key, r.max_key, r.nonnull, r.key_kind, r.sortedness, r.byte_len_min));
            assert_eq!((f.byte_len_max, f.computed), (r.byte_len_max, r.flags & pgrc_format::meta::STATSF_COMPUTED != 0));
            let d = StatsRecord::decode(&mut pgrc_format::wire::Cur::new(&b[..STATS_RECORD_LEN])).expect("decodes");
            assert_eq!((f.min_key, f.max_key, f.nonnull, f.key_kind), (d.min_key, d.max_key, d.nonnull, d.key_kind));
            assert!(ZoneFacts::of(&b[..STATS_RECORD_LEN - 1]).is_none());
        }
        let mut b = Vec::new(); // alloc: Cold
        StatsRecord::absent().encode_into(&mut b);
        assert_eq!(ZoneFacts::of(&b).map(|f| f.key_kind), Some(KeyKind::Absent.as_u8()));
        b[StatsRecord::OFF_KEY_KIND] = 9;
        assert!(ZoneFacts::of(&b).is_none(), "a bad key kind");
        b[StatsRecord::OFF_KEY_KIND] = KeyKind::Exact.as_u8();
        b[StatsRecord::OFF_SORTEDNESS] = Sortedness::Constant.as_u8() + 1;
        assert!(ZoneFacts::of(&b).is_none(), "a bad sortedness");
    }
}

/// One predicate over one granule's record (pgrc_meta's exact range logic
/// on the sign-extended int keys; SQL ternary: NULL never passes, an
/// `AllMatch` needs every row non-null). SRC-09: a prefix predicate
/// ([`ZonePred::prefix`]) reads a Coarse record by strict comparisons only
/// ([`prefix_verdict`]), an Exact one (P5-13) as exact keys.
fn zone_verdict(rec: &ZoneFacts, rows: u32, z: &ZonePred) -> u8 {
    let nonnull = rec.nonnull;
    if z.op == ZoneOp::NotNull {
        return if nonnull == 0 { V_NONE } else if nonnull == rows { V_ALL } else { V_MIXED };
    }
    if z.op == ZoneOp::IsNull {
        return if nonnull == rows { V_NONE } else if nonnull == 0 { V_ALL } else { V_MIXED };
    }
    if nonnull == 0 {
        return V_NONE;
    }
    // P5-11: a probe word is no key of the lane; only the bloom answers it
    if z.is_hashed() {
        return V_MIXED;
    }
    if z.prefix && rec.key_kind == KeyKind::Coarse.as_u8() {
        return prefix_verdict(rec, rows, z);
    }
    // P5-13: a prefix predicate over an Exact record (a memcmp-ordered fixed image of at most 8 bytes: macaddr, macaddr8,
    // whose key is the whole image) is a word's comparison of exact keys; a text prefix key's record is always Coarse
    if rec.key_kind != KeyKind::Exact.as_u8() {
        return V_MIXED;
    }
    let (min, max, c) = (rec.min_key, rec.max_key, z.value);
    let v = match z.op {
        ZoneOp::Lt => if max < c { V_ALL } else if min >= c { V_NONE } else { V_MIXED },
        ZoneOp::Le => if max <= c { V_ALL } else if min > c { V_NONE } else { V_MIXED },
        ZoneOp::Gt => if min > c { V_ALL } else if max <= c { V_NONE } else { V_MIXED },
        ZoneOp::Ge => if min >= c { V_ALL } else if max < c { V_NONE } else { V_MIXED },
        ZoneOp::Eq => if c < min || c > max { V_NONE } else if min == max { V_ALL } else { V_MIXED },
        ZoneOp::Ne => if min == max && min == c { V_NONE } else if c < min || c > max { V_ALL } else { V_MIXED },
        ZoneOp::NotNull | ZoneOp::IsNull | ZoneOp::InMember | ZoneOp::HashEq | ZoneOp::HashIn => V_MIXED,
    };
    if v == V_ALL && nonnull != rows { V_MIXED } else { v }
}

/// `col = c` / `col <> c` (`op`) over a text lane by the byte length `len` of `c`'s payload, against the granule's
/// non-NULL lengths: under a collation where equal strings are equal bytes (the lowerer declares a text equality's
/// predicate under no other), a value of another length is another value. So a record whose lengths miss `len` holds no
/// row equal to `c`: `=` matches none and `<>` every non-NULL row. The least length is 0 in a record without length
/// stats, where `len < min` never holds; the greatest is read only from a computed record (`STATSF_COMPUTED`) of a lane
/// that keeps them (the caller's `len`), since an uncomputed one reads 0. `Mixed` for any other operator.
fn len_verdict(rec: &ZoneFacts, rows: u32, op: ZoneOp, len: u32) -> u8 {
    if rec.nonnull == 0 {
        return V_NONE;
    }
    let miss = len < rec.byte_len_min || rec.computed && len > rec.byte_len_max;
    match op {
        ZoneOp::Eq if miss => V_NONE,
        ZoneOp::Ne if miss => if rec.nonnull == rows { V_ALL } else { V_MIXED },
        _ => V_MIXED,
    }
}

/// SRC-09: prefix predicate `z` over a granule's Coarse record (text under a bytes-order collation: each key is a value's
/// 8-byte zero-padded prefix, `a <= b` implies `key(a) <= key(b)`, ties across values): only a STRICT key comparison
/// decides. `key(x) < key(c)` proves `x < c` and `key(x) > key(c)` proves `x > c`, so: `=` matches no row when `key(c)`
/// lies outside [min, max] (and `<>` every non-NULL row); `<` / `<=` every row when max < key(c), none when min >
/// key(c); `>` / `>=` every row when min > key(c), none when max < key(c). A tie (a key equal to `key(c)`) is Mixed.
fn prefix_verdict(rec: &ZoneFacts, rows: u32, z: &ZonePred) -> u8 {
    let (min, max, c) = (rec.min_key, rec.max_key, z.value);
    let v = match z.op {
        ZoneOp::Lt | ZoneOp::Le => if max < c { V_ALL } else if min > c { V_NONE } else { V_MIXED },
        ZoneOp::Gt | ZoneOp::Ge => if min > c { V_ALL } else if max < c { V_NONE } else { V_MIXED },
        ZoneOp::Eq => if c < min || c > max { V_NONE } else { V_MIXED },
        ZoneOp::Ne => if c < min || c > max { V_ALL } else { V_MIXED },
        ZoneOp::NotNull | ZoneOp::IsNull | ZoneOp::InMember | ZoneOp::HashEq | ZoneOp::HashIn => V_MIXED,
    };
    if v == V_ALL && rec.nonnull != rows { V_MIXED } else { v }
}

#[cfg(test)]
mod prefix_verdict_tests {
    use super::{zone_verdict, KeyKind, ZoneFacts, ZoneOp, ZonePred, V_ALL, V_MIXED, V_NONE};

    /// SRC-09: every operator against every granule range of prefix keys, by brute force over short strings: a verdict
    /// of `AllMatch` / `NoneMatch` holds for every value whose key lies in the range (and a non-NULL granule), so ties,
    /// keys equal to the constant's and constants longer than 8 bytes never decide; an Exact record or a NULL-only granule
    /// answers as before.
    #[test]
    fn prefix_verdicts_hold_for_every_value_in_the_range() {
        let key = |s: &[u8]| pgrc_meta::key::memcmp_var_prefix_key(s).raw();
        let words: [&[u8]; 9] = [b"", b"a", b"ab", b"abcdefgh", b"abcdefghZ", b"abcdefgi", b"b", b"abcdefgh0", b"zz"];
        let ops = [ZoneOp::Lt, ZoneOp::Le, ZoneOp::Gt, ZoneOp::Ge, ZoneOp::Eq, ZoneOp::Ne];
        let holds = |op: ZoneOp, x: &[u8], c: &[u8]| match op {
            ZoneOp::Lt => x < c,
            ZoneOp::Le => x <= c,
            ZoneOp::Gt => x > c,
            ZoneOp::Ge => x >= c,
            ZoneOp::Eq => x == c,
            _ => x != c,
        };
        for lo in words {
            for hi in words.into_iter().filter(|h| key(*h) >= key(lo)) {
                let rec = ZoneFacts { min_key: key(lo), max_key: key(hi), nonnull: 10, byte_len_min: 0, byte_len_max: 0, computed: false, key_kind: KeyKind::Coarse.as_u8(), sorted: 0 };
                let inside: Vec<&[u8]> = words.into_iter().filter(|w| key(*w) >= key(lo) && key(*w) <= key(hi)).collect(); // alloc: Cold
                for c in words {
                    for op in ops {
                        let v = zone_verdict(&rec, 10, &ZonePred::prefix(0, op, key(c)));
                        if v == V_ALL {
                            assert!(inside.iter().all(|x| holds(op, x, c)), "{op:?} {c:?} over {lo:?}..{hi:?}");
                        } else if v == V_NONE {
                            assert!(inside.iter().all(|x| !holds(op, x, c)), "{op:?} {c:?} over {lo:?}..{hi:?}");
                        }
                    }
                }
            }
        }
        let rec = |kind: KeyKind, nonnull| ZoneFacts { min_key: key(b"m"), max_key: key(b"n"), nonnull, byte_len_min: 0, byte_len_max: 0, computed: false, key_kind: kind.as_u8(), sorted: 0 };
        // the empty string's key is i64::MIN, the code lane's sentinel: a prefix predicate still decides
        assert_eq!(zone_verdict(&rec(KeyKind::Coarse, 10), 10, &ZonePred::prefix(0, ZoneOp::Lt, key(b""))), V_NONE);
        assert_eq!(zone_verdict(&rec(KeyKind::Coarse, 10), 10, &ZonePred::prefix(0, ZoneOp::Gt, key(b"a"))), V_ALL);
        assert_eq!(zone_verdict(&rec(KeyKind::Coarse, 9), 10, &ZonePred::prefix(0, ZoneOp::Gt, key(b"a"))), V_MIXED, "a NULL row");
        assert_eq!(zone_verdict(&rec(KeyKind::Coarse, 0), 10, &ZonePred::prefix(0, ZoneOp::Gt, key(b"a"))), V_NONE, "NULL-only");
        assert_eq!(zone_verdict(&rec(KeyKind::Exact, 10), 10, &ZonePred::prefix(0, ZoneOp::Gt, key(b"a"))), V_ALL, "an Exact record: exact keys");
        assert_eq!(zone_verdict(&rec(KeyKind::Coarse, 10), 10, &ZonePred::word(0, ZoneOp::Gt, key(b"a"))), V_MIXED, "a word predicate never reads a Coarse record");
    }

    /// P5-13: a memcmp-ordered fixed image's order key, by brute force over uuids that tie on their 8-byte prefix (Coarse
    /// records, strict comparisons) and over macaddr images (Exact records, the whole image): every `AllMatch` holds for
    /// every image in the granule's key range and every `NoneMatch` for none, for every operator and constant.
    #[test]
    fn fixed_image_keys_decide_soundly() {
        let uuid = |hi: u64, lo: u64| -> [u8; 16] { let mut b = [0u8; 16]; b[..8].copy_from_slice(&hi.to_be_bytes()); b[8..].copy_from_slice(&lo.to_be_bytes()); b };
        let uuids = [uuid(0, 0), uuid(0, 9), uuid(5, 0), uuid(5, 1), uuid(5, u64::MAX), uuid(6, 0), uuid(u64::MAX, 0), uuid(u64::MAX, u64::MAX), uuid(1 << 63, 3)];
        let macs: [[u8; 6]; 6] = [[0; 6], [0, 0, 0, 0, 0, 1], [8, 0, 0x2b, 1, 2, 3], [8, 0, 0x2b, 1, 2, 4], [0x80, 0, 0, 0, 0, 0], [0xff; 6]];
        fn check<const N: usize>(images: &[[u8; N]], kind: KeyKind) {
            let key = |b: &[u8; N]| if N > 8 { pgrc_meta::key::memcmp_fixed_prefix_key(b).raw() } else { pgrc_meta::key::memcmp_fixed_exact_key(b).raw() };
            let holds = |op: ZoneOp, x: &[u8; N], c: &[u8; N]| match op {
                ZoneOp::Lt => x < c,
                ZoneOp::Le => x <= c,
                ZoneOp::Gt => x > c,
                ZoneOp::Ge => x >= c,
                ZoneOp::Eq => x == c,
                _ => x != c,
            };
            for lo in images {
                for hi in images.iter().filter(|h| key(h) >= key(lo)) {
                    let rec = ZoneFacts { min_key: key(lo), max_key: key(hi), nonnull: 10, byte_len_min: 0, byte_len_max: 0, computed: false, key_kind: kind.as_u8(), sorted: 0 };
                    let inside: Vec<&[u8; N]> = images.iter().filter(|w| key(w) >= key(lo) && key(w) <= key(hi)).collect(); // alloc: Cold
                    for c in images {
                        for op in [ZoneOp::Lt, ZoneOp::Le, ZoneOp::Gt, ZoneOp::Ge, ZoneOp::Eq, ZoneOp::Ne] {
                            let v = zone_verdict(&rec, 10, &ZonePred::prefix(0, op, key(c)));
                            assert!(v != V_ALL || inside.iter().all(|x| holds(op, x, c)), "{op:?} {c:?} over {lo:?}..{hi:?} AllMatch");
                            assert!(v != V_NONE || inside.iter().all(|x| !holds(op, x, c)), "{op:?} {c:?} over {lo:?}..{hi:?} NoneMatch");
                        }
                    }
                }
            }
        }
        check(&uuids, KeyKind::Coarse);
        check(&macs, KeyKind::Exact);
        // a granule of one macaddr (an all-equal granule) is decided whole by `=` on its Exact record, a uuid's never
        let one = |kind: KeyKind, k: i64| ZoneFacts { min_key: k, max_key: k, nonnull: 4, byte_len_min: 0, byte_len_max: 0, computed: false, key_kind: kind.as_u8(), sorted: 0 };
        let m = pgrc_meta::key::memcmp_fixed_exact_key(&macs[2]).raw();
        assert_eq!(zone_verdict(&one(KeyKind::Exact, m), 4, &ZonePred::prefix(0, ZoneOp::Eq, m)), V_ALL);
        let u = pgrc_meta::key::memcmp_fixed_prefix_key(&uuids[3]).raw();
        assert_eq!(zone_verdict(&one(KeyKind::Coarse, u), 4, &ZonePred::prefix(0, ZoneOp::Eq, u)), V_MIXED, "a prefix tie proves nothing");
        assert_eq!(zone_verdict(&one(KeyKind::Coarse, u), 4, &ZonePred::prefix(0, ZoneOp::Eq, u + 1)), V_NONE);
    }

    /// P5-11: a probe word (`HashEq` / `HashIn`) is no key of the lane: a record decides it only when the granule holds no
    /// non-NULL row (`=` never matches NULL), whatever its key kind and range, even a word equal to the range's bounds.
    #[test]
    fn probe_words_decide_only_null_granules() {
        for op in [ZoneOp::HashEq, ZoneOp::HashIn] {
            for kind in [KeyKind::Exact, KeyKind::Coarse, KeyKind::Absent] {
                let rec = |nonnull| ZoneFacts { min_key: 7, max_key: 7, nonnull, byte_len_min: 0, byte_len_max: 0, computed: false, key_kind: kind.as_u8(), sorted: 0 };
                for word in [7, 8, i64::MIN] {
                    let z = ZonePred::hashed(0, op, word);
                    assert!(z.is_hashed());
                    assert_eq!(zone_verdict(&rec(10), 10, &z), V_MIXED, "{op:?} {kind:?} {word}");
                    assert_eq!(zone_verdict(&rec(4), 10, &z), V_MIXED, "{op:?} {kind:?} {word}");
                    assert_eq!(zone_verdict(&rec(0), 10, &z), V_NONE, "{op:?} {kind:?} {word}: NULL-only");
                }
            }
        }
    }
}

#[cfg(test)]
mod null_verdict_tests {
    use super::{len_verdict, zone_verdict, KeyKind, ZoneFacts, ZoneOp, ZonePred, V_ALL, V_MIXED, V_NONE};

    /// A text equality by its constant's byte length, over every small length range and constant length (`''` is
    /// length 0): `=` matches no row and `<>` every non-NULL row exactly when the constant's length is below the least
    /// length, or above the greatest of a computed record; an uncomputed record's greatest length (0 in a record without
    /// stats) never decides. A computed range holds every value's length, so a decided verdict holds for every value in
    /// it. A NULL row keeps `<>` Mixed; an all-NULL record matches nothing; any other operator decides nothing.
    #[test]
    fn text_equalities_answer_from_the_length_range() {
        for computed in [false, true] {
            for min in 0..4u32 {
                for max in min..6u32 {
                    let rec = |nonnull| ZoneFacts { min_key: 0, max_key: 0, nonnull, byte_len_min: min, byte_len_max: max, computed, key_kind: KeyKind::Absent.as_u8(), sorted: 0 };
                    for len in 0..8u32 {
                        let decides = len < min || computed && len > max;
                        assert!(!(decides && computed && (min..=max).contains(&len)), "{min}..={max} {len}");
                        let at = format!("{computed} {min}..={max} len {len}"); // alloc: Cold
                        assert_eq!(len_verdict(&rec(10), 10, ZoneOp::Eq, len), if decides { V_NONE } else { V_MIXED }, "{at}");
                        assert_eq!(len_verdict(&rec(10), 10, ZoneOp::Ne, len), if decides { V_ALL } else { V_MIXED }, "{at}");
                        assert_eq!(len_verdict(&rec(6), 10, ZoneOp::Eq, len), if decides { V_NONE } else { V_MIXED }, "{at}");
                        assert_eq!(len_verdict(&rec(6), 10, ZoneOp::Ne, len), V_MIXED, "{at}: a NULL row fails <>");
                        assert_eq!(len_verdict(&rec(0), 10, ZoneOp::Ne, len), V_NONE, "{at}: all NULL");
                        for op in [ZoneOp::Lt, ZoneOp::Le, ZoneOp::Gt, ZoneOp::Ge, ZoneOp::InMember, ZoneOp::HashEq] {
                            assert_eq!(len_verdict(&rec(10), 10, op, len), V_MIXED, "{at} {op:?}");
                        }
                    }
                }
            }
        }
    }

    /// `IS NULL` and `IS NOT NULL` from the granule's non-NULL count alone, for any key kind: all-NULL, no-NULL and mixed
    /// granules, each the other's mirror.
    #[test]
    fn null_tests_answer_from_the_nonnull_count() {
        for kind in [KeyKind::Exact.as_u8(), KeyKind::Exact.as_u8().wrapping_add(1)] {
            for (nonnull, is_null, not_null) in [(0, V_ALL, V_NONE), (100, V_NONE, V_ALL), (40, V_MIXED, V_MIXED)] {
                let f = ZoneFacts { min_key: 1, max_key: 9, nonnull, byte_len_min: 0, byte_len_max: 0, computed: false, key_kind: kind, sorted: 0 };
                assert_eq!(zone_verdict(&f, 100, &ZonePred::word(0, ZoneOp::IsNull, 0)), is_null, "{nonnull} {kind}");
                assert_eq!(zone_verdict(&f, 100, &ZonePred::word(0, ZoneOp::NotNull, 0)), not_null, "{nonnull} {kind}");
            }
        }
    }
}

/// The zone census resident across statements (partitioned-record.md R4: a condition result over
/// the immutable parts): per `(list generation, relation, ask)` the survivor count of a predicate
/// set or the zone walk's order of a key column, keyed like `text_hosts::cond_table` — a new part
/// list is a new entry, a list with deletes is never stored; over `zone_store_bytes()` the store restarts.
enum ZoneEntry {
    Census(u64, Arc<[(u64, u64)]>),
    Order(Arc<ZoneOrder>),
    Whole(bool),
    /// `answered_parts`' count (`None`: the records cannot say).
    Parts(Option<u64>),
    /// `zone_extrema_of`'s answer for the list's generation (D580).
    Extrema(Option<(i64, i64)>),
    /// RD-width.walk: each granule's non-null value count of a nullable key column (`granule_nonnull`).
    Counts(Arc<[u64]>),
}

/// The zone walk's order over a list: the granules by ascending key minimum then granule, the
/// minima in that order, each granule's part.
pub struct ZoneOrder {
    pub order: Arc<[u32]>,
    pub mins: Vec<i64>,
    pub part_of: Vec<u16>,
}

impl ZoneOrder {
    /// From every granule's walk key in granule order (`i64::MIN` walks first): its minimum, or a descending walk's NOT of
    /// its maximum (`zone_walk_keys`); `mins` holds them in walk order.
    /// The granules sorted under a step, a tick per part mapped.
    pub fn of(mins: &[i64], list: &PartList) -> PgResult<ZoneOrder> {
        let mut step = cancelpoll::step("granule_source ZoneOrder::of: the granules in walk order");
        let mut order: Vec<u32> = (0..mins.len() as u32).collect(); // alloc: Cold
        let mut scratch = if order.len() > cancelpoll::SORT_RUN { vec![0u32; order.len()] } else { Vec::new() }; // alloc: Cold
        cancelpoll::sort_by(&mut step, &mut order, &mut scratch, |&a, &b| (mins[a as usize], a).cmp(&(mins[b as usize], b))).map_err(|_| err(WALK_CANCELED))?;
        let ordered: Vec<i64> = order.iter().map(|&g| mins[g as usize]).collect(); // alloc: Cold
        let mut part_of: Vec<u16> = Vec::with_capacity(mins.len()); // alloc: Cold
        for p in 0..list.len() {
            step.tick().map_err(|_| err(WALK_CANCELED))?;
            part_of.resize(list.end_of(p) as usize, p as u16);
        }
        Ok(ZoneOrder { order: Arc::from(order), mins: ordered, part_of })
    }

    fn bytes(&self) -> usize {
        self.order.len() * 14
    }
}

/// H32: the zone store's cap, from the machine: its share of the one process-cache budget, min(RAM / 64, the grant pool's
/// capacity), whichever is known (`cost::zone_store_cap`; the four process caches together stay within 9 / 32 of RAM). An entry
/// is at most 16 bytes a granule (an order 14 a granule, a census 16 a dead range, the counts 8 a granule), so the cap
/// bounds what a long-lived process keeps across relations and asks, never one statement's use (the flat 256 MiB it
/// replaces was one size on every machine).
pub fn zone_store_bytes() -> usize {
    crate::dict::text_hosts::reuse::store_bytes(gravity_program::cost::zone_store_cap)
}
static ZONE_STORE: Mutex<Option<(HashMap<(u64, Oid, u64), ZoneEntry>, usize)>> = Mutex::new(None);
static ZONE_BUILT: AtomicU64 = AtomicU64::new(0);
/// The answers built under `pgrust.zone_store = off` and so not kept (`zone_store_skipped`).
static ZONE_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// The zone answers built and not kept under `pgrust.zone_store = off` so far in this process.
pub fn zone_store_skipped() -> u64 {
    ZONE_SKIPPED.load(Ordering::Relaxed)
}

/// D3063 residual: the bytes the resident zone store holds now (the idle memory census).
/// D4398: on the memory ladder's pass, none while another thread holds the store (the pass leaves it out).
pub fn zone_store_held() -> usize {
    memledger::ladder_pass::lock_or_skip(&ZONE_STORE).map_or(0, |g| g.as_ref().map_or(0, |s| s.1))
}

/// Memory step 5: the zone store's reclaim for the memory budget (5b's reclaimer): past `bytes`, the store
/// restarts, as an insert past its cap restarts it. The bytes it held.
pub fn zone_store_clear(bytes: u64) -> u64 {
    // D4398: on the memory ladder's pass, nothing cleared while another thread holds the store
    let Some(mut g) = memledger::ladder_pass::lock_or_skip(&ZONE_STORE) else { return 0 };
    let Some((map, held)) = g.as_mut() else { return 0 };
    if *held as u64 <= bytes {
        return 0;
    }
    let freed = *held as u64;
    // G1-A: no single drop point (a scan may still hold an entry's Arc): the bytes leave the store's ledger account here
    zone_held(|c| c.free(freed));
    map.clear();
    *held = 0;
    freed
}

/// G1-A: the zone store's account in the memory ledger (its estimate of each entry's bytes moves there at the insert and
/// back to the clearing thread at a restart). C8 move 7: through the store's one Cache(ZoneStore) charge, under the store's
/// lock (opened at the first insert).
static ZONE_HELD: Mutex<Option<memledger::Charge>> = Mutex::new(None);

fn zone_held(f: impl FnOnce(&mut memledger::Charge)) {
    let owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::ZoneStore));
    let mut held = ZONE_HELD.lock().unwrap_or_else(|e| e.into_inner());
    f(held.get_or_insert_with(|| memledger::Charge::open(owner, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, 0)));
}

/// `[hits, built]` of the resident zone store so far in this process (the census's `zones=`).
pub fn zone_store_counts() -> [u64; 2] {
    memledger::value::flush();
    [memledger::value::stats(memledger::Consumer::ZoneStore).hits, ZONE_BUILT.load(Ordering::Relaxed)]
}

/// The word a zone ask's miss is timed by (memledger::value): its key.
#[inline]
fn zone_value_key(key: &(u64, Oid, u64)) -> u64 {
    key.0 ^ u64::from(key.1).rotate_left(32) ^ key.2.rotate_left(17)
}

fn zone_storable(list: &PartList) -> bool {
    list.parts().iter().all(|p| p.deletes.is_none())
}

/// `pgrust.zone_store = off`: no zone ask reads or writes the store (`zone_lookup` misses without a lookup, `zone_store`
/// keeps nothing), so each statement computes its answers from the parts' zone metadata again.
fn zone_store_off() -> bool {
    !crate::inputs::zone_store_on()
}

fn zone_lookup(key: &(u64, Oid, u64)) -> Option<ZoneEntry> {
    if zone_store_off() {
        return None;
    }
    memledger::value::lookup(memledger::Consumer::ZoneStore, zone_value_key(key), 0);
    let g = ZONE_STORE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(e) = g.as_ref().and_then(|s| s.0.get(key)) else {
        // the miss's price: the answer's build up to its `zone_store`, sampled
        memledger::value::arm(memledger::Consumer::ZoneStore, zone_value_key(key));
        return None;
    };
    memledger::value::hit(memledger::Consumer::ZoneStore);
    Some(match e {
        ZoneEntry::Census(n, d) => ZoneEntry::Census(*n, Arc::clone(d)),
        ZoneEntry::Order(o) => ZoneEntry::Order(Arc::clone(o)),
        ZoneEntry::Whole(w) => ZoneEntry::Whole(*w),
        ZoneEntry::Parts(n) => ZoneEntry::Parts(*n),
        ZoneEntry::Extrema(x) => ZoneEntry::Extrema(*x),
        ZoneEntry::Counts(c) => ZoneEntry::Counts(Arc::clone(c)),
    })
}

fn zone_store(key: (u64, Oid, u64), entry: ZoneEntry, bytes: usize) {
    if zone_store_off() {
        ZONE_SKIPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    ZONE_BUILT.fetch_add(1, Ordering::Relaxed);
    let mut g = ZONE_STORE.lock().unwrap_or_else(|e| e.into_inner());
    let (map, held) = g.get_or_insert_with(|| (HashMap::new(), 0)); // alloc: Cold
    if *held + bytes > zone_store_bytes() {
        let restart = *held as u64;
        zone_held(|c| c.free(restart));
        map.clear();
        *held = 0;
    }
    let kept = map.insert(key, entry).is_none(); // alloc: Cold
    if kept {
        *held += bytes + 64;
        zone_held(|c| c.commit((bytes + 64) as u64));
    }
    drop(g);
    // a built answer: the zone store's miss, the bytes it keeps (none for a racing builder's repeat)
    memledger::value::fault(memledger::Consumer::ZoneStore, zone_value_key(&key), if kept { (bytes + 64) as u64 } else { 0 });
}

/// A resident zone entry's key word: the ask's tag (1 census, 2 / 5 the ascending / descending walk order, 3 answers, 4
/// extrema, 6 / 7 the ascending / descending NULLS FIRST walk order, 8 a nullable column's non-null counts, 9 the answered
/// part count) and its words, each hashed as its eight bytes. The tag goes through the hash too: XORed into the basis, the
/// first byte of the first word cancelled it (`(2, [a])` and `(5, [a ^ 7])` were one key), so a descending order could
/// come back as another column's ascending one (PM13's DESC walk read the wrong granules).
fn zone_ask(tag: u64, words: impl IntoIterator<Item = u64>) -> u64 {
    words.into_iter().fold(fnv(0xcbf2_9ce4_8422_2325, &tag.to_le_bytes()), |h, w| fnv(h, &w.to_le_bytes()))
}

#[cfg(test)]
mod zone_ask_tests {
    use super::zone_ask;

    /// No two (tag, column) asks share a key: every tag the store uses over the first 1,600 attribute numbers.
    #[test]
    fn zone_asks_of_different_tags_or_columns_never_share_a_key() {
        let mut seen = std::collections::HashMap::new();
        for tag in 1..=9u64 {
            for attno in 0..1600u64 {
                if let Some(prev) = seen.insert(zone_ask(tag, [attno]), (tag, attno)) {
                    panic!("({tag}, {attno}) and {prev:?} share a key");
                }
            }
        }
        assert_ne!(zone_ask(5, [1]), zone_ask(2, [6]), "the descending order of column 1 is not column 6's ascending one");
    }
}

#[cfg(test)]
mod zone_store_switch_tests {
    use super::*;

    /// `pgrust.zone_store = off`: a lookup reads no entry (a resident one included) and a built answer is not kept; on
    /// again, nothing was kept while off.
    #[test]
    fn off_neither_reads_nor_writes_the_zone_store() {
        let kept = (0x7a0e_57a1_0001_u64, 4242 as Oid, zone_ask(9, [0x5eed]));
        let unkept = (0x7a0e_57a1_0002_u64, 4242 as Oid, zone_ask(9, [0x5eed]));
        guc_tables::backing::set_pgrust_zone_store(true);
        zone_store(kept, ZoneEntry::Parts(Some(3)), 16);
        assert!(matches!(zone_lookup(&kept), Some(ZoneEntry::Parts(Some(3)))), "on: the answer is kept and read");
        guc_tables::backing::set_pgrust_zone_store(false);
        let skipped = zone_store_skipped();
        assert!(zone_lookup(&kept).is_none(), "off: the resident entry is not read");
        zone_store(unkept, ZoneEntry::Parts(Some(5)), 16);
        assert!(zone_store_skipped() > skipped, "off: the built answer is counted as not kept");
        guc_tables::backing::set_pgrust_zone_store(true);
        assert!(zone_lookup(&unkept).is_none(), "off: nothing was kept");
    }
}

/// The selective decode's bar: at most `rows / SELECT_DIV` survivors, sampled over the first `SAMPLE_ROWS`. A rest lane
/// whose stream the cost home measured gathers by its own cost instead (`gather_pays`).
pub const SELECT_DIV: usize = 8;
pub const SAMPLE_ROWS: usize = 1024;

/// H09: the cost home's name for a by-value lane's values stream (`cost::pgrc::LaneDecode`), or `None` for a lane or
/// stream its fit (pgrc_codec select_fit) did not measure: a by-reference, code or numeric lane, a lane of width 1, a
/// VERBATIM stream stored at another width, any other encoding.
fn lane_decode(key: pgrc_format::abi::KernelKey, kind: u8, width: u8) -> Option<LaneDecode> {
    if kind != lane::KIND_BYVAL || !matches!(width, 2 | 4 | 8) {
        return None;
    }
    match key.encoding {
        e if e == EncodingId::ByteFor as u16 && (1..=8).contains(&key.width) => Some(LaneDecode::ByteFor { delta: key.width }),
        e if e == EncodingId::Verbatim as u16 && key.width == width => Some(LaneDecode::Verbatim),
        e if e == EncodingId::FforInterleave as u16 => Some(LaneDecode::Ffor),
        e if e == EncodingId::DeltaFor as u16 => Some(LaneDecode::DeltaFor),
        _ => None,
    }
}

/// The frames `n` asked rows from row `first` through row `last` reach: every frame between, at most one a row (an
/// upper bound when the rows skip a frame; exact for one run of rows, and for rows scattered over the granule).
fn frames_reached(first: u32, last: u32, n: usize) -> u64 {
    ((last / FRAME_VALUES - first / FRAME_VALUES) as u64 + 1).min(n as u64)
}

/// H09: whether a rest lane (stream `key`, lane `kind` and `width`) gathers the selection's `sel` rows (ascending, of a
/// `rows`-row granule) rather than decoding whole: by the fitted gather and whole-decode costs of its stream
/// (`LaneDecode::gather_pays`), or under the selective decode's bar where the fit did not measure it.
fn gather_pays(key: pgrc_format::abi::KernelKey, kind: u8, width: u8, rows: usize, sel: &[u32]) -> bool {
    let n = sel.len();
    match lane_decode(key, kind, width) {
        Some(d) if n > 0 => d.gather_pays(width, rows as u64, n as u64, frames_reached(sel[0], sel[n - 1], n)),
        _ => n * SELECT_DIV <= rows,
    }
}

#[cfg(test)]
mod gather_tests {
    use super::{frames_reached, gather_pays, lane_decode, LaneDecode, SELECT_DIV};
    use gravity_abi::ctx::lane;
    use pgrc_format::abi::KernelKey;
    use pgrc_format::enc::EncodingId;

    fn key(enc: EncodingId, width: u8) -> KernelKey {
        KernelKey { encoding: enc as u16, class: 0, width }
    }

    #[test]
    fn a_lane_names_its_stream_only_where_the_fit_measured_it() {
        let byval = lane::KIND_BYVAL;
        assert_eq!(lane_decode(key(EncodingId::ByteFor, 3), byval, 4), Some(LaneDecode::ByteFor { delta: 3 }));
        assert_eq!(lane_decode(key(EncodingId::Verbatim, 8), byval, 8), Some(LaneDecode::Verbatim));
        assert_eq!(lane_decode(key(EncodingId::FforInterleave, 0), byval, 2), Some(LaneDecode::Ffor));
        assert_eq!(lane_decode(key(EncodingId::DeltaFor, 0), byval, 4), Some(LaneDecode::DeltaFor));
        // a VERBATIM stream at another width, a 1-byte lane, a by-reference lane, another encoding: the bar
        assert_eq!(lane_decode(key(EncodingId::Verbatim, 4), byval, 8), None);
        assert_eq!(lane_decode(key(EncodingId::ByteFor, 1), byval, 1), None);
        assert_eq!(lane_decode(key(EncodingId::ByteFor, 1), lane::KIND_BYREF, 8), None);
        assert_eq!(lane_decode(key(EncodingId::Alp, 0), byval, 8), None);
        assert_eq!(lane_decode(key(EncodingId::PackedNumeric, 0), byval, 8), None);
    }

    #[test]
    fn the_frames_a_selection_reaches() {
        assert_eq!((frames_reached(0, 8191, 1024), frames_reached(0, 1023, 64), frames_reached(1000, 1100, 2), frames_reached(5, 5, 1)), (8, 1, 2, 1));
        assert_eq!(frames_reached(0, 8191, 3), 3, "at most one frame a row");
    }

    #[test]
    fn a_rest_lane_gathers_by_its_streams_cost_or_under_the_bar() {
        let (byval, rows) = (lane::KIND_BYVAL, 8192usize);
        let spread = |n: usize| (0..n as u32).map(|i| i * (rows / n) as u32).collect::<Vec<u32>>();
        // BYTE_FOR i32 at 1 byte: a 32nd of the rows gathers, a tenth decodes whole (under the old bar it gathered)
        let bf = key(EncodingId::ByteFor, 1);
        assert!(gather_pays(bf, byval, 4, rows, &spread(256)) && !gather_pays(bf, byval, 4, rows, &spread(819)));
        // BYTE_FOR i32 at 3 bytes: a fifth still gathers (the masked whole decode is dear)
        assert!(gather_pays(key(EncodingId::ByteFor, 3), byval, 4, rows, &spread(1638)));
        // FFOR: 256 rows in one frame gather; 512 spread over every frame decode whole
        let ff = key(EncodingId::FforInterleave, 0);
        assert!(gather_pays(ff, byval, 4, rows, &(0..256).collect::<Vec<u32>>()) && !gather_pays(ff, byval, 4, rows, &spread(512)));
        // a stream the fit did not measure: the bar, gather up to rows / SELECT_DIV
        let alp = key(EncodingId::Alp, 0);
        assert!(gather_pays(alp, byval, 8, rows, &spread(rows / SELECT_DIV)) && !gather_pays(alp, byval, 8, rows, &spread(rows / SELECT_DIV + 1)));
    }
}

fn mask_rows(mask: &mut [u64], from: usize, to: usize, keep: impl Fn(usize) -> bool) {
    // bounded("small-fixed: the mask words of one granule, at most VALIDITY_WORDS (128)")
    for (j, m) in mask.iter_mut().enumerate() {
        let base = from + j * 64;
        let n = (to - base).min(64);
        let mut bits = 0u64;
        // bounded("small-fixed: the rows of one 64-bit mask word, at most 64")
        for b in 0..n {
            bits |= (keep(base + b) as u64) << b;
        }
        *m &= bits;
    }
}

#[cfg(test)]
mod mask_ne_tests {
    use super::mask_rows;
    use super::raw::mask_ne_u32;

    #[test]
    fn mask_ne_u32_matches_scalar() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };
        for &(from, to) in &[(0usize, 64usize), (0, 200), (64, 320), (3, 3), (128, 128 + 64 * 5 + 17), (0, 1024)] {
            let codes: Vec<u32> = (0..to.max(1)).map(|_| (next() % 5) as u32).collect();
            let words = (to - from + 63) / 64;
            let init: Vec<u64> = (0..words).map(|_| next()).collect();
            for c in 0..5u32 {
                let (mut got, mut want) = (init.clone(), init.clone());
                mask_ne_u32(&mut got, from, to, &codes, c);
                mask_rows(&mut want, from, to, |i| codes[i] != c);
                assert_eq!(got, want, "from={from} to={to} c={c}");
            }
        }
    }
}

/// `0..64`: a full word's rows past its base (`words_to_sel`).
const IOTA32: [u32; 64] = {
    let mut a = [0; 64];
    let mut i = 0;
    // bounded("small-fixed: a const table of 64 entries, built at compile time")
    while i < 64 {
        a[i] = i as u32;
        i += 1;
    }
    a
};
const IOTA16: [u16; 64] = {
    let mut a = [0; 64];
    let mut i = 0;
    // bounded("small-fixed: a const table of 64 entries, built at compile time")
    while i < 64 {
        a[i] = i as u16;
        i += 1;
    }
    a
};

/// The set bits of `words` (each below the granule's rows) into `sel` / `sel16` in ascending order, their count: plain
/// stores into the reserved length and one length update each (a push per row reloads both vectors' length and capacity
/// through `&mut self`; this is a warm condition-cache hit's hottest host loop). A word's first two bits are stored
/// unconditionally, the count advancing by `m != 0`, so a sparse word takes no data-dependent branch (a selective window
/// keeping ~1% of rows: a word holds 0-3 bits, and the loop's exit per word was one mispredict); only the rest of a denser
/// word loops, and a full word stores its 64 rows as vector chunks. A store that does not advance the count lands where the
/// next kept row's does, or past the length.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn words_to_sel(words: &[u64], sel: &mut Vec<u32>, sel16: &mut Vec<u16>) -> usize {
    walk_sel::<true>(words, sel, sel16)
}

/// V4R-187: `words_to_sel` without `sel16`, which only a gathered decode reads (`decode_lane` of a lane that is not a code
/// lane, through `ensure_sel16`); a window over one code lane decodes it whole and never reads it.
fn words_to_sel32(words: &[u64], sel: &mut Vec<u32>, sel16: &mut Vec<u16>) -> usize {
    walk_sel::<false>(words, sel, sel16)
}

#[cfg(test)]
mod words_to_sel_tests {
    use super::words_to_sel;

    /// Every word shape the branchless form treats apart (empty, one bit, two, three, dense, full, the top bit) at every
    /// length a granule has, against a plain bit walk.
    #[test]
    fn words_to_sel_is_the_bit_walk() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };
        let (mut sel, mut sel16) = (Vec::with_capacity(8192), Vec::with_capacity(8192));
        for len in [1usize, 2, 3, 63, 64, 127, 128] {
            for density in 0..8 {
                let words: Vec<u64> = (0..len)
                    .map(|w| match (density + w) % 8 {
                        0 => 0,
                        1 => 1u64 << (next() % 64),
                        2 => (1u64 << (next() % 64)) | (1u64 << 63),
                        3 => next() & next() & next() & next(),
                        4 => next(),
                        5 => !0,
                        6 => 1u64 << 63,
                        _ => next() & next(),
                    })
                    .collect();
                let want: Vec<u32> = (0..len * 64).filter(|&i| words[i / 64] >> (i % 64) & 1 != 0).map(|i| i as u32).collect();
                let k = words_to_sel(&words, &mut sel, &mut sel16);
                assert_eq!(k, want.len(), "len={len} density={density}");
                assert_eq!(sel, want, "len={len} density={density}");
                assert!(sel16.iter().zip(&want).all(|(&a, &b)| a as u32 == b) && sel16.len() == k);
            }
        }
    }
}

/// A comparison zone predicate's rows as the inclusive `[lo, hi]` of the lane's values (`lo > hi`: none); `None` for `Ne`,
/// `NotNull` and `IsNull`, which no range holds.
fn pred_bounds(z: &ZonePred) -> Option<(i64, i64)> {
    let c = z.value;
    match z.op {
        ZoneOp::Lt => Some(c.checked_sub(1).map_or((1, 0), |h| (i64::MIN, h))),
        ZoneOp::Le => Some((i64::MIN, c)),
        ZoneOp::Gt => Some(c.checked_add(1).map_or((1, 0), |l| (l, i64::MAX))),
        ZoneOp::Ge => Some((c, i64::MAX)),
        ZoneOp::Eq => Some((c, c)),
        ZoneOp::Ne | ZoneOp::NotNull | ZoneOp::IsNull | ZoneOp::InMember | ZoneOp::HashEq | ZoneOp::HashIn => None,
    }
}

/// L5a: `lo <= cells[i] <= hi` into the mask words of rows `from..to`, one pass for a lane's fused range (`x >= a AND
/// x < b`: the miss path's row_select over a date range) where each predicate took its own; scalar (i8 / i64 lanes).
fn mask_range<T: Copy + Into<i64>>(mask: &mut [u64], from: usize, to: usize, cells: &[T], lo: i64, hi: i64) {
    mask_rows(mask, from, to, |i| (lo..=hi).contains(&cells[i].into()));
}

#[cfg(test)]
mod mask_range_tests {
    use super::raw::{mask_range_i16, mask_range_i32};
    use super::{int_pred, mask_range, pred_bounds, ZoneOp, ZonePred};

    /// Each fused range against its predicates applied one by one (int_pred), at the type's edges, empty and inverted
    /// ranges, and offsets that leave a partial last word.
    #[test]
    fn a_fused_range_is_its_predicates_one_by_one() {
        let mut seed = 0x6A09_E667_F3BC_C908u64;
        let mut next = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };
        let cuts = [i64::MIN, -40_000, i32::MIN as i64, -32_769, -32_768, -5, 0, 3, 32_767, 32_768, i32::MAX as i64, i64::MAX];
        let ops = [ZoneOp::Lt, ZoneOp::Le, ZoneOp::Gt, ZoneOp::Ge, ZoneOp::Eq];
        for &(from, to) in &[(0usize, 64usize), (0, 200), (64, 320), (3, 3), (128, 128 + 64 * 5 + 17), (0, 1024)] {
            let v32: Vec<i32> = (0..to.max(1)).map(|i| match i % 7 { 0 => i32::MIN, 1 => i32::MAX, 2 => (next() % 9) as i32 - 4, _ => next() as i32 }).collect();
            let v16: Vec<i16> = v32.iter().map(|&x| if x.unsigned_abs() < 8 { x as i16 } else { (x >> 16) as i16 }).collect();
            let words = (to - from).div_ceil(64);
            let init: Vec<u64> = (0..words).map(|_| next()).collect();
            for &a in &cuts {
                for &b in &cuts {
                    for (x, y) in [(ZoneOp::Ge, ZoneOp::Lt), (ops[(next() % 5) as usize], ops[(next() % 5) as usize])] {
                        let (za, zb) = (ZonePred::word(0, x, a), ZonePred::word(0, y, b));
                        let ((l1, h1), (l2, h2)) = (pred_bounds(&za).unwrap(), pred_bounds(&zb).unwrap());
                        let (lo, hi) = (l1.max(l2), h1.min(h2));
                        let (mut got, mut want) = (init.clone(), init.clone());
                        mask_range_i32(&mut got, from, to, &v32, lo, hi);
                        int_pred(&mut want, from, to, &v32, &za);
                        int_pred(&mut want, from, to, &v32, &zb);
                        assert_eq!(got, want, "i32 from={from} to={to} {x:?} {a} {y:?} {b}");
                        let (mut got, mut want) = (init.clone(), init.clone());
                        mask_range_i16(&mut got, from, to, &v16, lo, hi);
                        int_pred(&mut want, from, to, &v16, &za);
                        int_pred(&mut want, from, to, &v16, &zb);
                        assert_eq!(got, want, "i16 from={from} to={to} {x:?} {a} {y:?} {b}");
                        let (mut got, mut want) = (init.clone(), init.clone());
                        let v64: Vec<i64> = v32.iter().map(|&x| x as i64 * 3).collect();
                        mask_range(&mut got, from, to, &v64, lo, hi);
                        int_pred(&mut want, from, to, &v64, &za);
                        int_pred(&mut want, from, to, &v64, &zb);
                        assert_eq!(got, want, "i64 from={from} to={to} {x:?} {a} {y:?} {b}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod enc_mask_tests {
    use super::fetch::frame_mask;
    use super::raw::mask_delta;
    use super::{mask_range, mask_rows};

    fn rng(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    /// D3223 S1: `mask_delta` (the NEON body over whole words, the scalar tail) is `(d - dlo) mod 2^(8w) <= span` row by row,
    /// at each width, for runs that end on a whole and a partial word, dlo / span at 0 and the width's top.
    #[test]
    fn mask_delta_is_the_wrapped_compare() {
        let mut seed = 0x1319_8A2E_0370_7344u64;
        for w in [1u8, 2, 4] {
            let top = if w == 4 { u32::MAX } else { (1u32 << (8 * w)) - 1 };
            for &n in &[1024usize, 1000, 64, 65, 7] {
                let deltas: Vec<u8> = (0..n * w as usize).map(|_| rng(&mut seed) as u8).collect();
                let cell = |i: usize| deltas[i * w as usize..(i + 1) * w as usize].iter().rev().fold(0u32, |x, &b| x << 8 | b as u32);
                for _ in 0..24 {
                    let (a, b) = (rng(&mut seed) as u32 & top, rng(&mut seed) as u32 & top);
                    for (dlo, span) in [(a.min(b), a.max(b) - a.min(b)), (0, a), (a, 0), (0, top), (a, top - a)] {
                        let init: Vec<u64> = (0..n.div_ceil(64)).map(|_| rng(&mut seed)).collect();
                        let (mut got, mut want) = (init.clone(), init);
                        mask_delta(&mut got, &deltas, w, dlo, span);
                        mask_rows(&mut want, 0, n, |i| (cell(i).wrapping_sub(dlo) & top) <= span);
                        assert_eq!(got, want, "w={w} n={n} dlo={dlo} span={span}");
                    }
                }
            }
        }
    }

    /// D3223 S1: a range on a granule's stored deltas (`frame_mask` over each frame, as `enc_mask` walks them) keeps exactly
    /// the rows whose cell `reference + delta` the range on the decoded cells keeps (`mask_range`): references at the i64
    /// edges and inside, ranges below, across, inside and above a frame's values, empty and whole, windows that start
    /// inside the granule (a PSMA window, the selection's second pass) and end on a partial word.
    #[test]
    fn a_range_on_the_deltas_is_the_range_on_the_cells() {
        let mut seed = 0x243F_6A88_85A3_08D3u64;
        for w in [1u8, 2, 4] {
            let dmax = (1i64 << (8 * w)) - 1;
            for &rows in &[8192usize, 3000, 1024, 777, 1] {
                let nf = rows.div_ceil(1024);
                let refs: Vec<i64> = (0..nf).map(|f| match (f + w as usize) % 5 { 0 => i64::MIN, 1 => i64::MAX - dmax, 2 => -3, 3 => 1_000_000, _ => (rng(&mut seed) >> 2) as i64 - (1 << 61) }).collect();
                let deltas: Vec<Vec<u8>> = (0..nf)
                    .map(|f| {
                        let n = (rows - f * 1024).min(1024);
                        (0..n).flat_map(|i| {
                            let d = match i % 9 { 0 => 0, 1 => dmax as u64, _ => rng(&mut seed) & dmax as u64 };
                            d.to_le_bytes()[..w as usize].to_vec()
                        }).collect()
                    })
                    .collect();
                let cells: Vec<i64> = (0..rows).map(|i| {
                    let (f, j) = (i / 1024, i % 1024);
                    let d = deltas[f][j * w as usize..(j + 1) * w as usize].iter().rev().fold(0i64, |x, &b| x << 8 | b as i64);
                    refs[f] + d
                }).collect();
                for t in 0..40 {
                    let pick = |seed: &mut u64| match rng(seed) % 7 {
                        0 => i64::MIN,
                        1 => i64::MAX,
                        2 => cells[rng(seed) as usize % rows],
                        3 => refs[rng(seed) as usize % nf].saturating_sub(1),
                        4 => refs[rng(seed) as usize % nf].saturating_add(dmax + (rng(seed) % 3) as i64 - 1),
                        5 => cells[rng(seed) as usize % rows].saturating_add((rng(seed) % 5) as i64 - 2),
                        _ => rng(seed) as i64,
                    };
                    let (lo, hi) = (pick(&mut seed), pick(&mut seed));
                    let (lo, hi) = if t % 4 == 0 { (hi, lo) } else { (lo.min(hi), lo.max(hi)) };
                    for (from, to) in [(0, rows), (0, rows.min(1024)), (1024.min(rows / 64 * 64), rows), (rows / 128 * 64, rows - rows / 3)] {
                        if from >= to {
                            continue;
                        }
                        // apply_preds' words: no bit past `to`
                        let mut init: Vec<u64> = (0..to.div_ceil(64) - from / 64).map(|_| rng(&mut seed)).collect();
                        if to % 64 != 0 {
                            *init.last_mut().unwrap() &= (1u64 << (to % 64)) - 1;
                        }
                        let (mut got, mut want) = (init.clone(), init);
                        mask_range(&mut want, from, to, &cells, lo, hi);
                        for f in 0..nf {
                            if !frame_mask(&mut got, from, to, f * 1024, refs[f] as u64, &deltas[f], w, lo, hi) {
                                break;
                            }
                        }
                        assert_eq!(got, want, "w={w} rows={rows} from={from} to={to} lo={lo} hi={hi}");
                    }
                }
            }
        }
    }
}

fn int_pred<T: Copy + Into<i64>>(mask: &mut [u64], from: usize, to: usize, cells: &[T], z: &ZonePred) {
    let c = z.value;
    match z.op {
        ZoneOp::Lt => mask_rows(mask, from, to, |i| cells[i].into() < c),
        ZoneOp::Le => mask_rows(mask, from, to, |i| cells[i].into() <= c),
        ZoneOp::Gt => mask_rows(mask, from, to, |i| cells[i].into() > c),
        ZoneOp::Ge => mask_rows(mask, from, to, |i| cells[i].into() >= c),
        ZoneOp::Eq => mask_rows(mask, from, to, |i| cells[i].into() == c),
        ZoneOp::Ne => mask_rows(mask, from, to, |i| cells[i].into() != c),
        ZoneOp::NotNull | ZoneOp::IsNull | ZoneOp::InMember | ZoneOp::HashEq | ZoneOp::HashIn => {}
    }
}

/// The granule census over the resident zone columns (`pgrc_am::footer::ZoneColumn`, one `[min, max]`
/// per granule in granule order): one pass per predicate into a survivor bitmap, intersected, counted.
/// A part with deletes keeps every granule; a column without a zone key or a map of another granule
/// count excludes nothing (the count is the width rule's input, never an answer); resident per ask.
/// The survivor count and the granule ranges of the parts no survivor lies in (ascending, disjoint,
/// adjacent parts merged): the claim cursor never hands those out (the per-statement floor was 511
/// whole-part claims for 12 live parts).
pub fn zone_census(rel: &Relation<'_>, list: &PartList, attnos: &[i16], preds: &[ZonePred]) -> PgResult<(u64, Arc<[(u64, u64)]>)> {
    let storable = zone_storable(list);
    let key = (list.generation(), rel.rd_id, zone_ask(1, preds.iter().flat_map(|z| [attnos.get(z.lane as usize).copied().unwrap_or(-1) as u64, z.op as u64 | (z.prefix as u64) << 8, z.value as u64])));
    if storable {
        if let Some(ZoneEntry::Census(n, dead)) = zone_lookup(&key) {
            return Ok((n, dead));
        }
    }
    // D336: zone facts of another generation than the list's census nothing and are never stored
    let Some((n, dead)) = zone_census_walk(rel, list, attnos, preds)? else { return Ok((list.granules(), Arc::from(Vec::new()))) }; // alloc: Exec
    if storable {
        zone_store(key, ZoneEntry::Census(n, Arc::clone(&dead)), dead.len() * 16);
    }
    Ok((n, dead))
}

/// Whether a walk of whole parts answers every part of `list` from the catalog's part-grain records, no
/// part read: `stage_part`'s rule per part — no delete vector, and over `preds` (a site's static zone predicates) a
/// `NoneMatch` (skipped) or an `AllMatch` whose `answer` lanes' records are all exact (`answer`: one window). The caller
/// has checked that every attribute lane is a filter or an answer lane. `false` at the first part that would be read, a
/// record the catalog does not hold (a part's own Stats body is a read), or a shred / entries recipe. Kept in the zone
/// store per (generation, relation, ask): the catalog's records are the generation's (a walk of 511 parts cost ~22 µs).
pub fn answered_whole(list: &PartList, recipe: &GranuleRecipe, preds: &[ZonePred], answer: &[usize]) -> bool {
    if preds.is_empty() || !recipe.shred.is_empty() || recipe.entries.is_some() {
        return false;
    }
    let key = (list.generation(), recipe.rel, answer_ask(3, recipe, preds, answer));
    if let Some(ZoneEntry::Whole(w)) = zone_lookup(&key) {
        return w;
    }
    let whole = (0..list.len()).all(|p| part_answered(list, recipe, preds, answer, p) == Some(true));
    if zone_storable(list) {
        zone_store(key, ZoneEntry::Whole(whole), 8);
    }
    whole
}

/// The parts of `list` a walk of whole parts skips or answers from the catalog's part-grain records with no read
/// (`answered_whole`'s rule per part); `None` when the records cannot say: a shred or entries recipe, or a part whose filter
/// record the catalog does not hold (its own Stats body, read once the part is pinned, decides it). The runner's
/// whole-part claims for an answerable site turn on it (exec runner/plan.rs `claims_of`). Kept as `answered_whole` is.
pub fn answered_parts(list: &PartList, recipe: &GranuleRecipe, preds: &[ZonePred], answer: &[usize]) -> Option<u64> {
    if !recipe.shred.is_empty() || recipe.entries.is_some() {
        return None;
    }
    if preds.is_empty() {
        return Some(0);
    }
    let key = (list.generation(), recipe.rel, answer_ask(9, recipe, preds, answer));
    if let Some(ZoneEntry::Parts(n)) = zone_lookup(&key) {
        return n;
    }
    let n = (0..list.len()).try_fold(0u64, |n, p| Some(n + part_answered(list, recipe, preds, answer, p)? as u64));
    if zone_storable(list) {
        zone_store(key, ZoneEntry::Parts(n), 16);
    }
    n
}

/// `answered_whole` / `answered_parts`' zone-store ask: the predicates' (attno, op with the prefix and tallied bits, value),
/// then the answer lanes' attnos.
fn answer_ask(tag: u64, recipe: &GranuleRecipe, preds: &[ZonePred], answer: &[usize]) -> u64 {
    let attno = |k: usize| recipe.attnos.get(k).copied().unwrap_or(-1) as u16 as u64;
    zone_ask(tag, preds.iter().flat_map(|z| [attno(z.lane as usize), z.op as u64 | (z.prefix as u64) << 8 | (z.tallied as u64) << 9, z.value as u64]).chain(answer.iter().map(|&k| 1 << 32 | attno(k))))
}

/// Part `p` under `stage_part`'s rule from the catalog's records: `Some(true)` skipped (`NoneMatch`) or answered (`AllMatch`,
/// every `answer` lane's record exact), `Some(false)` read granule by granule, `None` a filter record the catalog does not hold.
fn part_answered(list: &PartList, recipe: &GranuleRecipe, preds: &[ZonePred], answer: &[usize], p: usize) -> Option<bool> {
    let record = |k: usize| -> Option<&[u8]> {
        let b = list.answer_record(p, *recipe.attnos.get(k)? as u32)?;
        let lg = (b.len() / STATS_RECORD_LEN).checked_sub(1)?;
        b.get(lg * STATS_RECORD_LEN..(lg + 1) * STATS_RECORD_LEN)
    };
    let spec = &list.parts()[p];
    if spec.deletes.is_some() || spec.rows > u32::MAX as u64 {
        return Some(false);
    }
    let mut all = true;
    // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
    for (i, z) in preds.iter().enumerate() {
        match zone_verdict(&record(z.lane as usize).and_then(ZoneFacts::of)?, spec.rows as u32, z) {
            V_NONE => return Some(true),
            V_ALL => {}
            // SRC-21: a tallied predicate over an answer lane answers a Mixed part from its tally (`tallied_mask`)
            _ if i < 63 && z.tallied && answer.contains(&(z.lane as usize)) => {}
            _ => all = false,
        }
    }
    let exact = |k: usize| record(k).and_then(|b| StatsRecord::decode(&mut Cur::new(b)).ok()).is_some_and(|r| r.flags & STATSF_COMPUTED != 0 && r.key_kind == KeyKind::Exact.as_u8());
    Some(all && answer.iter().all(|&k| exact(k)))
}

/// D336: the relation's latest committed generation (its resolve runs under no snapshot) when its parts are the list's
/// own open parts in manifest order, else `None`. Its zone columns speak for the list's granules only then: the list is
/// the statement snapshot's generation, and a granule-count match alone does not make two generations' granules the
/// same. `None` also for a list without resident parts (the tests').
fn list_generation(rel: &Relation<'_>, list: &PartList) -> PgResult<Option<Arc<pgrc_am::catalog::Generation>>> {
    let pgrc_am::catalog::Lookup::Gen(g) = visible(rel)? else { return Ok(None) };
    let same = g.parts.len() == list.parts.len() && g.parts.iter().zip(&list.parts).all(|(o, s)| s.part.as_ref().is_some_and(|p| Arc::ptr_eq(p, &o.part)));
    Ok(same.then_some(g))
}

/// The relation's latest committed generation in the process catalog (committed or this transaction's).
fn visible(rel: &Relation<'_>) -> PgResult<pgrc_am::catalog::Lookup> {
    let locator = rel.rd_locator.get();
    let slot: pgrc_am::catalog::RelSlot = (locator.spcOid, locator.dbOid, locator.relNumber as u64);
    let dir = pgrc_am::dirpath::table_dir_path(locator, rel.rd_backend); // alloc: Exec
    let check = pgrc_am::probe::SnapshotCommitCheck::new(None);
    let found = pgrc_am::catalog::resolve(rel, slot, &dir, &check)?;
    check.take_error()?;
    Ok(found)
}

/// D336's bracket: `read` of the latest committed generation's zone facts speaks for the list only when `gen` (the
/// list's generation) is `Some` both before the read and after it, the same (the latest committed
/// generation only moves forward, so every read between saw that one); `None` otherwise. Public for its test
/// (tests/d580_bracket.rs): a server-level test of a moved generation needs a retiring writer (M5e).
pub fn bracketed<T>(gen: &mut dyn FnMut() -> PgResult<Option<u64>>, read: impl FnOnce() -> PgResult<T>) -> PgResult<Option<T>> {
    let Some(g) = gen()? else { return Ok(None) };
    let x = read()?;
    Ok((gen()? == Some(g)).then_some(x))
}

/// The census walk over the list's zone columns (`list_zones`); `None` when it has none (D336: not the latest committed
/// generation's list).
fn zone_census_walk(rel: &Relation<'_>, list: &PartList, attnos: &[i16], preds: &[ZonePred]) -> PgResult<Option<(u64, Arc<[(u64, u64)]>)>> {
    let Some(zones) = list_zones(rel, list) else { return Ok(None) };
    zone_walk(&|a| Ok(zone_of(zones, a)), list, attnos, preds).map(Some)
}

/// `zone_census`'s dead ranges for a source without the relation (the serial seek): the same walk and zone
/// store entry over the zone columns `resolve` left on the list (read under D336's bracket there, so no walk-time
/// bracket); `None` when it left none (a list no `resolve` built, or of a generation not the latest committed).
fn zone_dead(list: &PartList, rel: Oid, attnos: &[i16], preds: &[ZonePred]) -> Option<Arc<[(u64, u64)]>> {
    let zones = list.zones.get()?;
    let key = (list.generation(), rel, zone_ask(1, preds.iter().flat_map(|z| [attnos.get(z.lane as usize).copied().unwrap_or(-1) as u64, z.op as u64 | (z.prefix as u64) << 8, z.value as u64])));
    let storable = zone_storable(list);
    if storable {
        if let Some(ZoneEntry::Census(_, dead)) = zone_lookup(&key) {
            return Some(dead);
        }
    }
    let (n, dead) = zone_walk(&|a| Ok(zone_of(zones, a)), list, attnos, preds).ok()?;
    if storable {
        zone_store(key, ZoneEntry::Census(n, Arc::clone(&dead)), dead.len() * 16);
    }
    Some(dead)
}

/// D1536: the rows of granule `lg` of part `p` (`rows` of them) its delete mask leaves, the rows PostgreSQL's scan reads.
fn visible_rows(list: &PartList, p: usize, lg: u32, rows: u32) -> u64 {
    let deleted = list.parts()[p].deletes.as_ref().and_then(|d| d.granule_mask(lg)).map_or(0, |m| m.iter().map(|w| w.count_ones() as u64).sum());
    (rows as u64).saturating_sub(deleted)
}

/// Whether granules `[lo, hi)` (one part's, non-empty) lie wholly inside one of `dead`'s ranges (ascending, merged).
fn dead_part(dead: &[(u64, u64)], lo: u64, hi: u64) -> bool {
    let i = dead.partition_point(|&(a, _)| a <= lo);
    lo < hi && i > 0 && dead[i - 1].1 >= hi
}

/// The census walk over the zone columns `col_of` gives: the survivor count and the ranges of the parts no survivor
/// lies in.
fn zone_walk(col_of: &dyn Fn(u32) -> PgResult<Option<Arc<ZoneColumn>>>, list: &PartList, attnos: &[i16], preds: &[ZonePred]) -> PgResult<(u64, Arc<[(u64, u64)]>)> {
    let n = list.granules() as usize;
    let mut alive: Vec<u64> = vec![!0u64; n.div_ceil(64)]; // alloc: Exec
    // a tick per predicate and per part, one per BATCH granules tested
    let mut step = cancelpoll::step("granule_source zone_walk: the census over the zone columns");
    for z in preds {
        step.tick().map_err(|_| err(WALK_CANCELED))?;
        // the zone columns hold no NULL counts: an `IS NULL` keeps every granule; they hold exact keys, never a prefix's
        if z.prefix || z.op == ZoneOp::IsNull {
            continue;
        }
        let Some(&attno) = attnos.get(z.lane as usize) else { continue };
        let Some(col) = col_of(attno as u32)? else { continue };
        let ranges = col.ranges();
        if ranges.len() != n {
            continue;
        }
        let c = z.value;
        for (g, &(min, max)) in ranges.iter().enumerate() {
            step.batch(g).map_err(|_| err(WALK_CANCELED))?;
            let none = min > max
                || match z.op {
                    ZoneOp::Lt => min >= c,
                    ZoneOp::Le => min > c,
                    ZoneOp::Gt => max <= c,
                    ZoneOp::Ge => max < c,
                    ZoneOp::Eq => c < min || c > max,
                    ZoneOp::Ne => min == max && min == c,
                    ZoneOp::NotNull | ZoneOp::IsNull | ZoneOp::InMember | ZoneOp::HashEq | ZoneOp::HashIn => false,
                };
            if none {
                alive[g / 64] &= !(1u64 << (g % 64));
            }
        }
    }
    // a part the zone maps cannot speak for keeps every granule
    for (p, spec) in list.parts().iter().enumerate() {
        step.tick().map_err(|_| err(WALK_CANCELED))?;
        if spec.deletes.is_some() || spec.rows > u32::MAX as u64 {
            for g in list.starts()[p] as usize..list.end_of(p) as usize {
                step.batch(g).map_err(|_| err(WALK_CANCELED))?;
                alive[g / 64] |= 1u64 << (g % 64);
            }
        }
    }
    let tail = n % 64;
    if tail != 0 {
        if let Some(last) = alive.last_mut() {
            *last &= (1u64 << tail) - 1;
        }
    }
    let mut dead: Vec<(u64, u64)> = Vec::new(); // alloc: Exec
    for p in 0..list.len() {
        step.tick().map_err(|_| err(WALK_CANCELED))?;
        let (lo, hi) = (list.starts()[p], list.end_of(p));
        if lo == hi || (lo..hi).any(|g| alive[g as usize / 64] >> (g % 64) & 1 != 0) {
            continue;
        }
        match dead.last_mut() {
            Some(last) if last.1 == lo => last.1 = hi,
            _ => dead.push((lo, hi)), // alloc: Exec
        }
    }
    Ok((alive.iter().map(|w| w.count_ones() as u64).sum(), Arc::from(dead)))
}

/// A column's extrema over the keyed granules from the resident zone summary (`lo > hi`: none): the latest committed
/// generation's (`visible_zone_column`), an estimate for the planner and the lowerer; a statement's bound is
/// `zone_extrema_of`'s.
pub fn zone_extrema(rel: &Relation<'_>, attno: u32) -> PgResult<Option<(i64, i64)>> {
    Ok(visible_zone_column(rel, attno)?.and_then(|c| extrema(&c)))
}

fn extrema(col: &ZoneColumn) -> Option<(i64, i64)> {
    let s = col.summary();
    (s.lo <= s.hi).then_some((s.lo, s.hi))
}

/// D580: `zone_extrema` for the list's own granules, from its zone columns (`list_zones`, as `zone_minima`): the dense
/// fold's domain (the owner's `t3_dense::word`, the dense front's bound) raises on a key outside it, so another
/// generation's extrema (a retiring writer's: a merge, a compaction) are no bound; `None` without them, as with no zone
/// column. Kept in the zone store per (generation, relation, column) like the census.
pub fn zone_extrema_of(rel: &Relation<'_>, list: &PartList, attno: u32) -> PgResult<Option<(i64, i64)>> {
    let (storable, key) = (zone_storable(list), (list.generation(), rel.rd_id, zone_ask(4, [attno as u64])));
    if let Some(ZoneEntry::Extrema(x)) = storable.then(|| zone_lookup(&key)).flatten() {
        return Ok(x);
    }
    let Some(x) = list_zones(rel, list).map(|z| zone_of(z, attno).and_then(|c| extrema(&c))) else { return Ok(None) };
    if storable {
        zone_store(key, ZoneEntry::Extrema(x), 16);
    }
    Ok(x)
}

/// The zone walk's order key from the resident zone column (R4): every granule's key minimum in granule
/// order, `i64::MIN` without keys (`min > max`: it walks first); `None` without a map of every granule.
pub fn zone_minima(rel: &Relation<'_>, list: &PartList, attno: u32) -> PgResult<Option<Vec<i64>>> {
    zone_walk_keys(rel, list, attno, false)
}

/// `zone_minima` for a walk in either direction: descending, every granule's NOT of its key maximum (a DESC run's key word
/// is the NOT of the ascending one, so the walk's ascending order and its bound test hold as they are).
pub fn zone_walk_keys(rel: &Relation<'_>, list: &PartList, attno: u32, desc: bool) -> PgResult<Option<Vec<i64>>> {
    // D336: the minima are the walk's lower bounds; another generation's are not the list's
    let Some(col) = list_zones(rel, list).and_then(|z| zone_of(z, attno)) else { return Ok(None) };
    let r = col.ranges();
    if r.len() != list.granules() as usize {
        return Ok(None);
    }
    Ok(Some(r.iter().map(|&(lo, hi)| if lo > hi { i64::MIN } else if desc { !hi } else { lo }).collect())) // alloc: Exec
}

/// The zone walk's order of `attno` over the list from the resident zone column, resident per
/// `(generation, relation, column)`; `None` without a map of every granule (`granule_minima` then).
pub fn zone_order(rel: &Relation<'_>, list: &PartList, attno: u32) -> PgResult<Option<Arc<ZoneOrder>>> {
    zone_walk_order(rel, list, attno, false)
}

/// `zone_order` in either direction (`zone_walk_keys`), resident per direction too.
pub fn zone_walk_order(rel: &Relation<'_>, list: &PartList, attno: u32, desc: bool) -> PgResult<Option<Arc<ZoneOrder>>> {
    let storable = zone_storable(list);
    let key = (list.generation(), rel.rd_id, zone_ask(if desc { 5 } else { 2 }, [attno as u64]));
    if storable {
        if let Some(ZoneEntry::Order(o)) = zone_lookup(&key) {
            return Ok(Some(o));
        }
    }
    let Some(mins) = zone_walk_keys(rel, list, attno, desc)? else { return Ok(None) };
    let o = Arc::new(ZoneOrder::of(&mins, list)?); // alloc: Cold
    if storable {
        zone_store(key, ZoneEntry::Order(Arc::clone(&o)), o.bytes());
    }
    Ok(Some(o))
}

/// RD-width.walk, the row walk's reach: the granules a serial walk of `n` rows visits over the list's zone column (the
/// planner's PM13 census by rows, run on the list itself). In walk order, the keyed granules' rows are summed until they
/// reach `n`; row n - 1's key is then at most the farthest edge among them (a maximum ascending, the NOT of a minimum
/// descending), so the walk stops before the first granule whose walk key lies past it. A keyless granule (`lo > hi`) walks
/// first and adds no value row. Every granule when the keyed rows never reach `n`. `None` without a zone map of every
/// granule, or over a list with deletes (its rows are fewer than the counts: the reach would come out short). Over a
/// nullable key (a NULLS LAST walk: its NULL rows come after every value) a granule adds its non-null values only, from
/// its stats record (`nonnull_counts`), so a granule's NULLs never stop the census early.
pub fn zone_walk_reach(rel: &Relation<'_>, list: &PartList, attno: u32, desc: bool, n: u64) -> PgResult<Option<usize>> {
    let Some(col) = list_zones(rel, list).and_then(|z| zone_of(z, attno)) else { return Ok(None) };
    if !zone_storable(list) || col.ranges().len() != list.granules() as usize {
        return Ok(None);
    }
    let Some(o) = zone_walk_order(rel, list, attno, desc)? else { return Ok(None) };
    // D2838: a NOT VALID not-null constraint sets attnotnull over rows that may hold NULLs; only a VALID one (the
    // relcache's attnullability) says the column holds none
    let i = attno as usize - 1;
    let nullable = rel.rd_att.attrs.get(i).is_some() && rel.rd_att.compact_attr(i).attnullability != types_tuple::ATTNULLABLE_VALID;
    let counts = if nullable { Some(nonnull_counts(rel, list, attno)?) } else { None };
    let rows_of = |g: u32| {
        if let Some(c) = &counts {
            return c.get(g as usize).copied().unwrap_or(0);
        }
        let p = o.part_of[g as usize] as usize;
        let (spec, lg) = (&list.parts()[p], (g as u64 - list.starts()[p]) as u32);
        match &spec.part {
            Some(part) => pgrc_format::geom::rows_in_granule_at(part.rows(), part.grain(), lg) as u64,
            None => (spec.rows / (spec.granules.max(1) as u64)).max(1),
        }
    };
    Ok(Some(walk_reach(&o.order, &o.mins, col.ranges(), rows_of, desc, n)))
}

/// A nullable key column's non-null value count per granule (`granule_nonnull`), resident per `(generation, relation,
/// column)` under tag 8; read once per list generation. Only a storable list comes here (`zone_walk_reach`).
fn nonnull_counts(rel: &Relation<'_>, list: &PartList, attno: u32) -> PgResult<Arc<[u64]>> {
    let key = (list.generation(), rel.rd_id, zone_ask(8, [attno as u64]));
    if let Some(ZoneEntry::Counts(c)) = zone_lookup(&key) {
        return Ok(c);
    }
    let c: Arc<[u64]> = Arc::from(granule_nonnull(list, attno)?); // alloc: Cold
    zone_store(key, ZoneEntry::Counts(Arc::clone(&c)), c.len() * 8);
    Ok(c)
}

/// Each granule's non-null value count of column `attno`, in list order, from its stats record (the resident zone
/// column keeps each granule's (lo, hi) only); a granule without a record counts its rows.
pub fn granule_nonnull(list: &PartList, attno: u32) -> PgResult<Vec<u64>> {
    let mut out = Vec::with_capacity(list.granules() as usize); // alloc: Exec
    // a tick per part (its open and Stats body read), one per BATCH granules
    let mut step = cancelpoll::step("granule_source granule_nonnull: the parts' Stats records");
    for (p, spec) in list.parts().iter().enumerate() {
        step.tick().map_err(|_| err(WALK_CANCELED))?;
        let opened;
        let part: &OpenPart = match &spec.part {
            // FASTADD: a column the part lacks: its stand-in's records
            Some(real) => list.lane_part(p, attno, real)?,
            None => {
                opened = OpenPart::open(open_io(&spec.io).map_err(read_err)?, &spec.expect).map_err(read_err)?; // alloc: Cold
                &opened
            }
        };
        let body = stats_body(part, attno, 0, FirstTouch::GranuleWalk)?;
        for g in 0..spec.granules {
            step.batch(g as usize).map_err(|_| err(WALK_CANCELED))?;
            let rows = pgrc_format::geom::rows_in_granule_at(part.rows(), part.grain(), g);
            let rec = body.as_deref().and_then(|b| {
                let off = g as usize * STATS_RECORD_LEN;
                StatsRecord::decode(&mut Cur::new(b.get(off..off + STATS_RECORD_LEN)?)).ok()
            });
            out.push(rec.map_or(rows, |r| r.nonnull.min(rows)) as u64);
        }
    }
    Ok(out)
}

/// RD-dict.entries: whether open part `part`'s column holds a NULL, from its Stats `body` (granule records first,
/// `STATS_RECORD_LEN` each: a granule whose `nonnull` count is under its rows); `None` when a granule has no record.
fn part_has_null(part: &OpenPart, body: &[u8]) -> Option<bool> {
    let granules = pgrc_format::geom::granule_count_at(part.rows(), part.grain());
    // one tick per BATCH granules; a cancel answers `None` (the part walks its rows: the caller's next tick stops it)
    let mut step = cancelpoll::step("granule_source part_has_null: a part's Stats records");
    for g in 0..granules as u32 {
        step.batch(g as usize).ok()?;
        let rows = pgrc_format::geom::rows_in_granule_at(part.rows(), part.grain(), g);
        let off = g as usize * STATS_RECORD_LEN;
        let r = body.get(off..off + STATS_RECORD_LEN).and_then(|b| StatsRecord::decode(&mut Cur::new(b)).ok())?;
        if r.nonnull < rows {
            return Some(true);
        }
    }
    Some(false)
}

/// `zone_walk_reach` over a walk order (`order` / `mins` as `ZoneOrder` holds them), every granule's zone range and rows.
pub fn walk_reach(order: &[u32], mins: &[i64], ranges: &[(i64, i64)], rows_of: impl Fn(u32) -> u64, desc: bool, n: u64) -> usize {
    let (mut rows, mut far) = (0u64, i64::MIN);
    // one tick per BATCH granules; a cancel answers every granule (the plain read's reach: the caller's next tick stops it)
    let mut step = cancelpoll::step("granule_source walk_reach: the walk's census");
    for (i, &g) in order.iter().enumerate() {
        if step.batch(i).is_err() {
            return order.len();
        }
        let (lo, hi) = ranges[g as usize];
        if lo > hi {
            continue;
        }
        (rows, far) = (rows + rows_of(g), far.max(if desc { !lo } else { hi }));
        if rows >= n {
            return mins.partition_point(|&m| m <= far);
        }
    }
    order.len()
}

/// PL-06: the walk order of a NULLS FIRST run over a nullable key (`granule_walk_keys` with `nulls_first`: the resident
/// zone column keeps no null counts, the granules' stats records do), resident per `(generation, relation, column,
/// direction)` under its own tags; read once per list generation.
pub fn nulls_first_walk_order(rel: &Relation<'_>, list: &PartList, attno: u32, desc: bool) -> PgResult<Arc<ZoneOrder>> {
    let storable = zone_storable(list);
    let key = (list.generation(), rel.rd_id, zone_ask(if desc { 7 } else { 6 }, [attno as u64]));
    if let Some(ZoneEntry::Order(o)) = storable.then(|| zone_lookup(&key)).flatten() {
        return Ok(o);
    }
    let o = Arc::new(ZoneOrder::of(&granule_walk_keys(list, attno, desc, true)?, list)?); // alloc: Cold
    if storable {
        zone_store(key, ZoneEntry::Order(Arc::clone(&o)), o.bytes());
    }
    Ok(o)
}

/// Whether the granules' zone ranges of `attno` are disjoint and ascending in scan order (a key
/// value never spans two non-adjacent granules): the dominance form's gate (`t3::MISC_DOMINANT`).
pub fn granules_ordered(rel: &Relation<'_>, attno: u32) -> PgResult<bool> {
    let Some(col) = visible_zone_column(rel, attno)? else { return Ok(false) };
    let r = col.ranges();
    Ok(!r.is_empty() && r.iter().all(|&(lo, hi)| lo <= hi) && r.windows(2).all(|w| w[0].1 <= w[1].0))
}

/// The exact key range of `attno` over the list's parts (this snapshot's parts, not the newest generation's), from each
/// part's part-grain Stats record (the catalog's answer, else its Stats body's last record): a part of NULLs alone adds
/// nothing; `None` when a part holding a value has no exact bounded record, or no part holds one. The dense domain's
/// bound (`Fact::DenseLo` / `DenseHi`): deleted rows only widen it.
pub fn part_extrema(list: &PartList, attno: u32) -> PgResult<Option<(i64, i64)>> {
    let (mut lo, mut hi) = (i64::MAX, i64::MIN);
    // a tick per part: one may be opened and its Stats body read
    let mut step = cancelpoll::step("granule_source part_extrema: the parts' Stats records");
    for (p, spec) in list.parts().iter().enumerate() {
        step.tick().map_err(|_| err(WALK_CANCELED))?;
        let rec = match list.answer_record(p, attno) {
            Some(b) => StatsRecord::decode(&mut Cur::new(b)).ok(),
            None => {
                let opened;
                let part: &OpenPart = match &spec.part {
                    // FASTADD: a column the part lacks: its stand-in's record
                    Some(real) => list.lane_part(p, attno, real)?,
                    None => {
                        opened = OpenPart::open(open_io(&spec.io).map_err(read_err)?, &spec.expect).map_err(read_err)?; // alloc: Cold
                        &opened
                    }
                };
                let why = if list.lacks(p, attno) { FirstTouch::StandIn } else { FirstTouch::NoAnswers };
                let body = stats_body(part, attno, 0, why)?;
                body.as_deref().and_then(|b| {
                    let at = (b.len() / STATS_RECORD_LEN).checked_sub(1)? * STATS_RECORD_LEN;
                    StatsRecord::decode(&mut Cur::new(&b[at..at + STATS_RECORD_LEN])).ok()
                })
            }
        };
        match rec {
            Some(r) if r.nonnull == 0 => {}
            Some(r) if r.key_kind == KeyKind::Exact.as_u8() && (r.min_key, r.max_key) != (i64::MIN, i64::MAX) && r.min_key <= r.max_key => {
                (lo, hi) = (lo.min(r.min_key), hi.max(r.max_key));
            }
            _ => return Ok(None),
        }
    }
    Ok((lo <= hi).then_some((lo, hi)))
}

/// The exact key minimum of every granule of `attno` over the list, in global granule order
/// (the zone walk's order key, step 5): `i64::MIN` for a granule without an exact record (it
/// walks first, which is always sound). The resident parts are read as they are; a part the
/// list holds unopened (the tests' in-memory parts) is opened for the read.
pub fn granule_minima(list: &PartList, attno: u32) -> PgResult<Vec<i64>> {
    granule_walk_keys(list, attno, false, false)
}

/// `granule_minima` for a walk in either direction: descending, the NOT of each exact key maximum (`zone_walk_keys`).
/// `nulls_first` (PL-06, a NULLS FIRST walk over a nullable key): a granule holding a NULL — its record's non-null count
/// under its rows — keys `i64::MIN` too, so it walks first and no bound lies under it (a NULLS FIRST run's NULL row
/// bounds at `i64::MIN`, exec merge.rs `run_bound`). A deleted row still counts: such a granule only walks early.
pub fn granule_walk_keys(list: &PartList, attno: u32, desc: bool, nulls_first: bool) -> PgResult<Vec<i64>> {
    let mut out = Vec::with_capacity(list.granules() as usize); // alloc: Exec
    // a tick per part (its open and Stats body read), one per BATCH granules
    let mut step = cancelpoll::step("granule_source granule_walk_keys: the parts' Stats records");
    for (p, spec) in list.parts().iter().enumerate() {
        step.tick().map_err(|_| err(WALK_CANCELED))?;
        let opened;
        let part: &OpenPart = match &spec.part {
            // FASTADD: a column the part lacks: its stand-in's records
            Some(real) => list.lane_part(p, attno, real)?,
            None => {
                opened = OpenPart::open(open_io(&spec.io).map_err(read_err)?, &spec.expect).map_err(read_err)?; // alloc: Cold
                &opened
            }
        };
        let body = stats_body(part, attno, 0, FirstTouch::GranuleWalk)?;
        for g in 0..spec.granules {
            step.batch(g as usize).map_err(|_| err(WALK_CANCELED))?;
            let rows = pgrc_format::geom::rows_in_granule_at(part.rows(), part.grain(), g);
            let min = body.as_deref().and_then(|b| {
                let off = g as usize * STATS_RECORD_LEN;
                let rec = StatsRecord::decode(&mut Cur::new(b.get(off..off + STATS_RECORD_LEN)?)).ok()?;
                let nulls = nulls_first && rec.nonnull < rows;
                (rec.key_kind == KeyKind::Exact.as_u8() && rec.nonnull > 0 && !nulls).then_some(if desc { !rec.max_key } else { rec.min_key })
            });
            out.push(min.unwrap_or(i64::MIN));
        }
    }
    Ok(out)
}


pub(crate) fn open_io(spec: &PartIoSpec) -> ReadResult<Box<dyn PartIo>> {
    match spec {
        PartIoSpec::Path(p) => Ok(Box::new(VfsPartIo::open(p)?)), // alloc: Exec
        PartIoSpec::Mem { bytes, dev, ino } => Ok(Box::new(MemPartIo::new(bytes.to_vec(), *dev, *ino))), // alloc: Cold
    }
}

/// A code lane's dictionary in one part: the writer's, or — a verbatim part — the byte-ranked codes
/// of its distinct values with their `Fp128` table (`text_hosts::Ranks`, a property of the part
/// cached process-wide; hosts.md "The verbatim part's code lane"); the kernels see one lane kind.
enum Dict {
    Part(DictHandle),
    Ranked { table: crate::dict::text_hosts::FpPin, epoch: u64 },
}

impl Dict {
    fn ncodes(&self) -> u32 {
        match self {
            Dict::Part(d) => d.ncodes(),
            Dict::Ranked { table, .. } => table.ranks().expect("ranks").ncodes(),
        }
    }

    fn epoch64(&self) -> u64 {
        match self {
            Dict::Part(d) => d.epoch64(),
            Dict::Ranked { epoch, .. } => *epoch,
        }
    }
}

/// A code lane's dictionary in one part.
pub struct DictEntry {
    /// D4482 b: from the moment a list's warm cell keeps the entry, its own allocation is the part cache's (`warm`; its
    /// handle's images were already): dropped first, wherever the entry's last holder drops it.
    held: OnceLock<memledger::Charge>,
    dict: Dict,
}

impl DictEntry {
    fn new(dict: Dict) -> DictEntry {
        DictEntry { held: OnceLock::new(), dict }
    }

    /// D4482 b: `e`, just allocated on this thread, offered to a list's warm cell: its `Arc` block on the part cache's
    /// holder (dropped with it when the cell keeps another statement's entry).
    fn warm(e: &Arc<DictEntry>) {
        // D4552 (a): and its handle's own lists (its open's, on this thread), which go wherever its last holder drops it
        let loose = match &e.dict {
            Dict::Part(d) => d.loose_bytes() as usize,
            Dict::Ranked { .. } => 0,
        };
        let bytes = core::mem::size_of::<DictEntry>() + 2 * core::mem::size_of::<usize>() + loose;
        let _ = e.held.set(memledger::Charge::open(memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache)), memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, bytes as u64));
    }
}

/// A source's table lanes: the once-cells (their own small region) and the tables' bump region;
/// `part_gen` bumps at every part change, which frees every cell (by construction: a
/// table lives while its part's windows are processed, never across statements).
struct TableState {
    cells: Vec<u64>,   // alloc: Node
    once: Option<gravity_dict::OnceTable>,
    /// The tables' chunks: a build that overflows the last opens a new one (the earlier tables
    /// stay where a kernel holds them); all but the largest are dropped at the part change.
    bufs: Vec<Vec<u64>>, // alloc: Node
    used: usize,
    part_gen: u64,
    part: usize,
    fmgr: Option<gravity_fn::FmgrCall>,
    built: u64,
    /// The process-resident `Fp128` tables published for this part (R2): pinned while a lane points at them.
    pinned: Vec<crate::dict::text_hosts::FpPin>, // alloc: Node
    /// The `TableFn::Program` tables allocated for the program's sweep, published at `LANE_DONE` (a sweep the statement never finishes drops the claim as failed).
    pending: Vec<(usize, gravity_dict::once::Builder, *const gravity_dict::table::Header)>, // alloc: Node
    /// D3115: this part's shared sweeps this source takes chunks of (published at `LANE_DONE`), and the swept ones its
    /// published headers read, pinned for the part.
    sweeps: Vec<SweepShare>, // alloc: Node
    swept: Vec<Arc<SharedSweep>>, // alloc: Node
    /// (2') DK1-MISEST: the `FLAG_ADAPTIVE` call tables this part published unfilled (table lane index, header), their rows'
    /// fill read at the part change (`enter`, `ADAPT_SAMPLE` entries), and the table lanes this source builds hosted from
    /// then on (bit `i`: its last unfilled part's rows filled at least `ADAPT_HOSTED_PCT` % of the entries).
    adapt_lazy: Vec<(usize, *const gravity_dict::table::Header)>, // alloc: Node
    adapt_hosted: u64,
}

impl Drop for TableState {
    /// D3250: an unpublished `Builder` marks its cell `FAILED` as it drops, and the cell lives in `cells`, which the
    /// fields' declaration order frees first.
    fn drop(&mut self) {
        self.pending.clear();
    }
}

const ADAPT_SAMPLE: u32 = gravity_program::cost::pgrc::ADAPT_SAMPLE_ENTRIES;
const ADAPT_HOSTED_PCT: u64 = gravity_program::cost::pgrc::ADAPT_HOSTED_PCT;

const TABLE_REGION_MIN: usize = gravity_program::cost::pgrc::TABLE_REGION_MIN_BYTES;


impl TableState {
    /// D3213: the chunks and lists kept between statements (`Source::kept_bytes`).
    fn kept_bytes(&self) -> usize {
        use gravity_driver::vec_bytes as v;
        v(&self.cells) + v(&self.bufs) + self.bufs.iter().map(v).sum::<usize>() + v(&self.pinned) + v(&self.pending) + v(&self.sweeps) + v(&self.swept) + v(&self.adapt_lazy)
    }

    /// D4536: a new chunk of at least `bytes` for the tables, charged to `held` as it is made: the lane that makes it
    /// runs after the window's sync, and a build fills it (up to 100+ MiB on a big part) until the next window's.
    fn push_chunk(&mut self, bytes: usize, held: &mut HeldCharge) {
        let words = bytes.div_ceil(8);
        self.bufs.push(vec![0u64; words]); // alloc: Node charged held
        self.used = 0;
        held.grow(words as u64 * 8);
    }

    /// D3115: the shared sweeps this source still takes chunks of, left to their other sources.
    fn abandon_sweeps(&mut self) {
        // bounded("key-columns: one shared sweep per table lane of the part, at most MaxTupleAttributeNumber (1664) columns")
        for s in self.sweeps.iter_mut() {
            s.abandon();
        }
        self.sweeps.clear();
    }

    fn new() -> TableState {
        TableState { cells: Vec::new(), once: None, bufs: Vec::new(), used: 0, part_gen: 0, part: usize::MAX, fmgr: None, built: 0, pinned: Vec::new(), pending: Vec::new(), sweeps: Vec::new(), swept: Vec::new(), adapt_lazy: Vec::new(), adapt_hosted: 0 } // alloc: Node
    }

    /// Enter part `p`: a new generation, the region rewound. D1747: a once-cell per table lane of the recipe (`slots`): a
    /// generation holds one part's tables, at most one cell per lane, so a claim never finds the cells full.
    fn enter(&mut self, p: usize, slots: usize) {
        if self.part == p && self.once.is_some() {
            return;
        }
        // (2'): the previous part's unfilled adaptive tables, read before this part's builds reuse their region
        // bounded("key-columns: one unfilled adaptive table per table lane of the previous part (at most MaxTupleAttributeNumber (1664) columns), each sampling at most ADAPT_SAMPLE_ENTRIES (4096) entries")
        for (i, h) in self.adapt_lazy.drain(..) {
            // `h`: a header this source built in its own region for the previous part (`build_table`'s, a `Program` Fp128
            // table), not yet reused (`used` resets below)
            let filled = super::raw::adapt_fill(h, ADAPT_SAMPLE);
            if i < 64 && filled.is_some_and(|(f, n)| n > 0 && f * 100 >= n * ADAPT_HOSTED_PCT) {
                self.adapt_hosted |= 1 << i;
            }
        }
        self.part = p;
        self.part_gen += 1;
        self.used = 0;
        self.pinned.clear();
        self.pending.clear();
        self.abandon_sweeps();
        self.swept.clear();
        if self.bufs.len() > 1 {
            let largest = self.bufs.iter().map(Vec::len).max().unwrap_or(0);
            self.bufs.retain(|b| b.len() == largest);
            self.bufs.truncate(1);
        }
        if self.once.is_none() {
            let slots = slots.max(1) as u32;
            self.cells = vec![0u64; gravity_dict::OnceTable::region_bytes(slots).div_ceil(8)]; // alloc: Node
            let mut r = self.cells_region();
            self.once = gravity_dict::OnceTable::in_region(&mut r, slots).ok();
        }
        if let Some(o) = &self.once {
            o.begin_query(self.part_gen);
        }
    }
}

static TABLES_BUILT: AtomicU64 = AtomicU64::new(0);
static TABLES_SHARED: AtomicU64 = AtomicU64::new(0);

/// The census: table lanes built this process.
pub fn tables_built() -> u64 {
    TABLES_BUILT.load(Ordering::Relaxed)
}

/// The census: table lanes served from a statement's shared store (built by another source).
pub fn tables_shared() -> u64 {
    TABLES_SHARED.load(Ordering::Relaxed)
}

/// A lane's Stats records for one part: the part-grain record in the catalog's answers (the
/// granule records not yet read), the whole body read from the part (a declared first touch),
/// or absent.
enum StatsBody {
    None,
    Catalog,
    Owned(Box<[u8]>),
}

/// The recipe a granule source reads: its own, or (sortmerge T2) the one its merged source owns and lends to each of its
/// part streams' sources (merged.rs `Member`), which it outlives: a stream's source holds no recipe of its own.
pub(crate) enum RecipeHome {
    Own(GranuleRecipe),
    Lent(&'static GranuleRecipe),
}

impl core::ops::Deref for RecipeHome {
    type Target = GranuleRecipe;
    #[inline(always)]
    fn deref(&self) -> &GranuleRecipe {
        match self {
            RecipeHome::Own(r) => r,
            RecipeHome::Lent(r) => r,
        }
    }
}

/// D4554: the parts a walk over a source's kept cursors visits (`GranuleRangeSource::each_kept_of`).
#[derive(Clone, Copy)]
enum KeptParts {
    /// Every part the source holds cursors over (`held_parts`).
    All,
    /// The parts this bind opened (`touched`, the `kept_parked` switch).
    Touched,
    /// The parts it holds cursors over that a bind after bind `n` opened (`part_bind` past `n`: the `kept_parked` park).
    Since(u64),
}

pub struct GranuleRangeSource {
    /// CONVERT wave 2: the kept buffers' charge (`held_bytes`), first so it drops before them.
    held: HeldCharge,
    recipe: RecipeHome,
    /// The recipe's registry index (`at_site`; `usize::MAX`: none, the recipe's own cell) and the verdict cell bound
    /// for this execution (D300: the statement's `Resolved::cells_from`, else the recipe's).
    site: usize,
    cell: Arc<VerdictCell>,
    /// The statement's `Resolved` (null unbound) and this relation's index in it.
    resolved: *const Resolved,
    rel: usize,
    list: Option<Arc<PartList>>,
    order: Option<Arc<[u32]>>,
    /// Per (part, lane) this source's decode cursor, opened at the part's first staged granule; dropped on a new list generation.
    /// Boxed (D538): the table is parts x lanes (~1,000 parts on hits) per worker source, so its grow and drop touch 8-byte slots.
    cursors: Vec<Option<Box<StreamCursor<'static>>>>,
    /// D2985 (`release.rs`): the claim's first granule not yet credited to its part; the lowest part this source may hold
    /// cursors over; each part's release epoch as this source last entered it.
    credit_from: u64,
    credit_part: usize,
    cursor_low: usize,
    seen_epoch: Vec<u32>, // alloc: Node
    lanes_open: Vec<bool>,
    /// D4554 (the `kept_decay` bound): the bind count at each part's last lanes or dictionary open (`binds` then), and
    /// this source's binds.
    part_bind: Vec<u64>,
    binds: u64,
    /// D4554: the parts this source may hold cursors over (`note_part` adds a part at its first open, `held_mark` says it
    /// is listed; `prune_held` drops the ones whose cursors all closed): the walks over its kept cursors each statement
    /// (bind, unbind, the slot depot's park and take) visit these, not every part of the list.
    held_parts: Vec<u32>,
    held_mark: Vec<bool>,
    /// D4554 (the `kept_parked` switch): the parts this bind opened (`note_part`'s first call for a part in a bind): the
    /// statement's end, the depot's park and a window's owner move visit their cursors only; `touch_live` while the bind
    /// that listed them has not ended (`unbind_pg`).
    touched: Vec<u32>,
    touch_live: bool,
    /// D4554 (`kept_parked`): `binds` at this source's last slot-depot park (`kept_uses_to`). Each unbind moves the uses of
    /// the cursors its own bind opened to the session, and a worker slot binds its sources again at each section of a
    /// statement, so a park follows several binds: it walks every part a bind opened since the last park (`part_bind` past
    /// this), not only the last bind's `touched`.
    park_bind: u64,
    /// D4554 (`kept_parked`): the last bind's owner (`charge::context()`), whether an unbind left uses pending there, and
    /// `binds` at the last park or settle (the parts opened since hold every pending use: a Node's own sources never park).
    bind_owner: Option<memledger::Owner>,
    park_pending: bool,
    settle_bind: u64,
    /// D4554 (`kept_parked`): bound as a worker slot's source (`BindingRef::ordinal`): only those park in the slot depot.
    slot_bound: bool,
    /// The lane whose cursor answers a granule's row count: the first that is no shred lane (a part may not elect a
    /// shred lane's path, and has no cursor for it), else lane 0 (`granule_rows` then asks the part).
    rows_lane: usize,
    /// Per lane its face in the bound list's `Faces` (set at bind): the column's attno, or its shred lane's slot.
    faces: Vec<u32>,
    cursor_generation: u64,
    cur_part: usize,
    /// The serial seek's zone census: the granule ranges of whole parts no zone predicate keeps, jumped
    /// before the part is armed, looked up or pinned (a claim's range never holds one: the runner cut them).
    dead: Option<Arc<[(u64, u64)]>>,
    /// The census this bind took (its predicates' bound values are the bind's), once: `None` until a serial seek asks.
    dead_memo: Option<Option<Arc<[(u64, u64)]>>>,
    /// The current part's registry pin (kept from the janitor's eviction while its granules decode). A warm list's part
    /// that its part-grain records answer or skip whole is never pinned (the serial answer: `faces_known`).
    pin: Option<PartPin>,
    /// `(part, measuring runtime-filter slots)` whose Stats bodies `granule_stats` has read under the bound list (they stay in
    /// its faces).
    stats_read: (usize, u8),
    datums: Vec<u64>,
    /// Per lane, a by-reference lane's image arena (empty otherwise).
    arenas: Vec<Vec<u8>>,
    /// The numeric lanes of the staged granule decoded as images (a part stored verbatim): published `KIND_BYREF`.
    lane_byref: LaneBits,
    /// Per lane, the staged window's values in place (`in_place`: a root VERBATIM body inside the loaded extent image),
    /// or null: the lane's datum slot. Cleared with every granule `stage` visits.
    inplace: Vec<*const u8>,
    valid: Vec<u64>,
    sel: Vec<u32>,
    /// The selection as the reader's survivor list, the survivors' dense datums, the predicates' row mask.
    sel16: Vec<u16>,
    /// V4R-187: `sel16` holds `sel`'s rows (the words' walk leaves it for `ensure_sel16`, the first gathered decode).
    sel16_ok: bool,
    dense: Vec<u64>,
    mask: Box<[u64; VALIDITY_WORDS]>,
    /// The staged window carries `sel` (a delete mask or the predicates' survivors).
    selected: bool,
    all_valid: Vec<bool>,
    dv: Box<[u64; DV_GRANULE_WORDS]>,
    has_dv: bool,
    lo: u64,
    hi: u64,
    g: u64,
    /// skewsplit (D3320): the pool claim's cut this source adopted (`Source::claim_cut`): asked at each granule step
    /// (`stage`), it moves `hi` to the midpoint of what is left; `None` outside a cut claim.
    cut: Option<gravity_driver::claimcut::CutToken>,
    /// `granules_skipped` when the cut was adopted: the claim's own skipped units are the count past it (its pace counts
    /// only the units it read, D3247).
    cut_skip0: u64,
    staged: u64,
    nrows: usize,
    granules_decoded: u64,
    granules_skipped: u64,
    granules_selected: u64,
    /// The static predicates (`zone`), then this execution's word-valued ones (`bind_pg`).
    zone: Vec<ZonePred>,
    nzone: usize,
    /// The predicates `zone` registered, and this execution's word-valued ones after them (D289, `bind_pg`); `nzone` is 0
    /// for an execution whose zone road is forced off.
    nzone_decl: usize,
    /// D3156: this execution keeps the site's verbatim parts' code tables in its own cells (kept.rs, set at bind).
    kept_mode: bool,
    /// D3038 (A): this execution codes its length-only lanes' verbatim parts by length in the site's own cells (kept.rs
    /// `open_lens`, set at bind).
    lens_mode: bool,
    /// The static predicates alone (`Source::zone`, at the node's build): each bind appends its word-valued ones to them.
    nzone_static: usize,
    /// NEW-72: a static predicate is bound (`ZonePred::bound`), and this execution's bind found every one's value.
    has_bound: bool,
    bound_ok: bool,
    /// D3009: the site's disjunctions (`Source::zone_any`: group `i` ends at `any_ends[i]`), their bound values the
    /// protocol's slots `any_base + i`; `any_ok`: this execution bound every one (else none skips a granule).
    any: Vec<ZonePred>,
    any_ends: Vec<u16>,
    any_base: usize,
    any_ok: bool,
    /// SRC-21: the static predicates marked `tallied` over an answer lane, as a verdict mask (`pred_bit`; a predicate past
    /// the 63rd shares the top bit with others, so it is never one): a window whose only `Mixed` predicates these are is
    /// answered from the records' tally (`tallies`).
    tallied_mask: u64,
    /// SRC-21: the staged granule's verdict is `Mixed` on tallied predicates alone (`verdict`).
    tallied_win: bool,
    /// PS1-05: the predicates after this execution's bind (static and word-valued); each seek appends the open-time ones
    /// (`key_zone`) to them.
    nzone_bind: usize,
    /// sortpath (code-keyed zones): this execution's text constants of its prefix predicates (`VerdictCell::code_images`),
    /// and, for part `zcode_part`, each zone predicate's compare on the part's codes (`zcode_preds`, by predicate index:
    /// an exact word predicate over the CodeZones ranges; `None` where the part or the predicate has none).
    zcode_images: Vec<CodeImage>,
    zcode_preds: Vec<Option<ZonePred>>,
    zcode_part: usize,
    /// Each zone predicate's constant byte length for `len_verdict` (by predicate index, `NO_LEN` where there is none): a
    /// text equality's (`=` / `<>` prefix predicate over a varlena lane that keeps length stats) whose constant this
    /// execution holds (`zcode_images`, every image for its lane, op and key of one length).
    zlen: Vec<u32>,
    /// For part `ne_part` under this bind's constants (reset at each bind), a record lane's `<>` constant placed by
    /// `row_code` (by predicate index below `ROW_PREDS`, valid where bit `zi` of `ne_placed` is set): its code, `u32::MAX`
    /// where the part lacks it, `None` where unplaceable.
    ne_codes: [Option<u32>; ROW_PREDS],
    ne_placed: u64,
    ne_part: usize,
    /// D4554 (D3160's part verdict once a part; `PGRUST_GRAVITY_PART_DECIDES_MEMO=1`, `part_dec_on`): for part `part_dec_p`,
    /// each zone predicate's part-grain verdict (`part_decides`, by predicate index): a verdict, `PD_NOTHING` (the part's
    /// record decides nothing) or `PD_ASK` (no record when asked: each granule asks again, as without the memo).
    part_dec_on: bool,
    part_dec_p: usize,
    part_dec: Vec<u8>,
    /// PS1-05: this execution's open-time zone predicates (`VerdictCell::key_zone`, under the zone road) and the values the
    /// last seek read for them (`None` before the first seek of a bind).
    key_zone: Vec<KeyZone>,
    key_vals: Option<Vec<Option<ZonePred>>>,
    /// The site's verdict enables for this execution (`VerdictCell::LEGACY`: the zone verdicts alone).
    enables: u64,
    /// The staged granule's PSMA candidate rows (H02c); `None` = every row.
    win: Option<(u32, u32)>,
    /// The condition cache (H02i): the finished fingerprint (`[0, 0]` = off), the pinned part's entry, the words buffer
    /// (the cached words on a hit, the IR's on a miss), the verdict lane's flags (`V_ALL` covered, `V_MIXED` a miss), a
    /// selection from cached words, and the granule whose words publish at the next advance.
    cond_mode: guc_tables::ConditionCacheMode,
    cond_fp: [u64; 2],
    cond_entry: Option<(usize, Arc<crate::ops::scan::pgrc::cond_cache::Entry>)>,
    cond_words: Box<[u64; VALIDITY_WORDS]>,
    cond_hit: u8,
    cond_mixed: bool,
    cond_due: Option<(Arc<crate::ops::scan::pgrc::cond_cache::Entry>, u32, usize)>,
    /// Road code.sparse's table form (H08, fetch.rs `sparse_stage`): the part and its verdict table, once read.
    sparse_tab: Option<(usize, crate::dict::text_hosts::FpPin)>,
    /// The part whose granule showed the table form no dominant base (`usize::MAX`: none): its granules decode whole.
    sparse_dense: usize,
    /// H06: the part whose row selection by the code verdict's table kept too many of a granule's sampled rows (`usize::MAX`:
    /// none): its later granules sample without the table (one sample's cost per part on a qual most rows pass).
    table_dense: usize,
    /// The runtime filters (`VerdictCell::rt_at`), one per slot (D3245).
    rt: [RtSlot; RT_SLOTS],
    /// D3245: the slot whose filter cut last, asked first.
    rt_first: usize,
    /// D2856 (colskip): the cell's runtime-filter generation `rt` was taken under (`u64::MAX`: none taken this bind). A
    /// part entered under a later one takes the cell's filters again (a rerun set's build ran again, or its set re-opened).
    rt_gen: u64,
    /// decode.late (H01e): the lanes `lane_fill` decodes this execution, those pending in the staged window (its part,
    /// granule and rows), each lane's decoded rows, and its images' arena `(chunk, used)`: chunk 0 is `arenas[k]`, the
    /// later ones `late_more[k]`; a chunk never moves inside a window, so earlier fills' images stay put.
    late: u64,
    late_pend: u64,
    late_g: (usize, u32, usize),
    late_done: Vec<u64>,
    late_at: Vec<(usize, usize)>,
    late_more: Vec<Vec<Vec<u8>>>,
    late_sel: Vec<u32>,
    /// M6 D2: the rows `guard_fill` fills.
    guard_sel: Vec<u32>,
    /// M6 D2: the late guarded jsonb column lanes (bit k; `late_guards` under `late`): no claim run reads them.
    guard_late: u64,
    /// jsonops: the path lanes (bit k) whose invalid rows in a part that elected the path are SQL NULL
    /// (`null_when_absent`): `guard_fill` leaves those rows `{}`. Never a presence lane: an unflagged part publishes
    /// it all-invalid although it elected the path, and its array-root documents answer `?` true.
    guard_null: u64,
    late_sel16: Vec<u16>,
    /// Each lane's rows decoded this window (`late_done`'s count), and the rows its fills asked so far with the span they
    /// reach (the highest asked row + 1) and the fills' calls and the frames each call reached: the cost `late_fill`
    /// switches to the whole lane on (`late_dense`).
    late_cnt: Vec<u32>,
    late_req: Vec<u32>,
    late_span: Vec<u32>,
    late_calls: Vec<(u32, u32)>,
    /// decode.packed: this execution's candidate lanes, and the staged window's packed lanes (0 = not packed); their
    /// `PackedFrames` sit past the lane slots in `datums`.
    packed: u64,
    packed_win: u64,
    /// `read_claim_run`'s runs of admitted granules (kept at its high-water mark: steady claims allocate nothing).
    claim_runs: Vec<(u32, u32)>,
    /// The runs of those the condition cache does not know, for the lanes a hit leaves undecoded.
    claim_miss_runs: Vec<(u32, u32)>,
    /// D4315 (`unpin_window`): this execution reads ahead by lane and passes the extents behind its windows (a releasing
    /// scan in granule order: its relation has the statement's release cells, no walk order, no sample).
    unpin: bool,
    /// D4315: the part and band `pass_behind` last ran at; per lane the granule its read-ahead reaches (`u32::MAX`: the
    /// lane reads none); the claim's run split (`read_claim_run`'s `split` / `unread`); a lane's clipped runs.
    unpin_at: (usize, u32),
    ahead: Vec<u32>,
    ahead_split: (bool, bool),
    ahead_runs: Vec<(u32, u32)>,
    filter_lanes: LaneBits,
    answer_lanes: LaneBits,
    /// The lanes whose Stats bodies are loaded: the predicate lanes and the answer lanes (fixed at `zone`).
    zone_lanes: LaneBits,
    /// The zone lanes read for their part-grain records alone (`ne_trial`): no granule Stats body, meta section or code
    /// zones.
    record_lanes: LaneBits,
    /// The `<>` lanes on trial (`ne_trial`): their predicates (by index below 63), those of them over lanes an `AllMatch`
    /// elides or answers, those a granule record of the parts since the last pin paid for, whether a granule of those
    /// parts was asked, and those that paid for none in the last part asked before it.
    ne_watch: u64,
    ne_elides: u64,
    ne_paid: u64,
    ne_asked: bool,
    ne_dry: u64,
    /// M2-ZONEREAD card A: the zone-predicate lanes whose granule verdicts come from the relation footer's resident zone
    /// map (`PartList::zones`: exact per-granule [min, max] of a word key), not from a Stats body read per part; never an
    /// answer lane (its records answer windows). `map_cols[k]` is lane k's map, every granule of the list.
    mapped: LaneBits,
    map_cols: Vec<Option<Arc<[(i64, i64)]>>>,
    /// The mapped lanes of part `map_all_part` whose every row is non-NULL (its catalog part record: nonnull == rows): only
    /// those take an `AllMatch` from the map, which bounds the non-NULL values alone.
    map_all: LaneBits,
    map_all_part: usize,
    /// The mapped lanes whose Stats / Bloom / PSMA sections the entered claim reads with its run: a granule the map leaves
    /// `Mixed` on a predicate bloom or PSMA could decide.
    meta_need: LaneBits,
    verdict: u8,
    /// Lanes every one of whose zone predicates is `AllMatch` over the staged granule (or part).
    lane_all: LaneBits,
    elided: LaneBits,
    /// The decode's lane sets: decoded whole first (the code and predicate lanes), then the rest.
    first: LaneBits,
    rest: LaneBits,
    /// D3223 S1: the staged window's zone lanes `row_select` compares on their stored deltas (`enc_stage`; out of `first`).
    enc: Vec<self::fetch::EncLane>,
    enc_set: LaneBits,
    /// D3223 S2: this window's `row_select` selection is exact (`sel_exact`, reset at every `stage` step before the
    /// selection; `enc_fell`: an encoded lane left its rows unmasked), and the staged window publishes AllMatch for it
    /// (`sel_all`: reset at every `stage` step).
    sel_exact: bool,
    enc_fell: bool,
    sel_all: bool,
    scratch_a: LaneBits,
    scratch_b: LaneBits,
    answered: bool,
    entries_staged: bool,
    /// Road dict.entries' runs (granule_source/entry_runs.rs): the consumer's offer was accepted this execution; each part's choice, first
    /// seen (0 unseen, `RUNS_TAKEN`, `RUNS_ROWS`); the runs taken, read by the owner phase.
    runs_ok: bool,
    runs_seen: Vec<u8>,
    /// RD-dict.entries (`entries_nulls`): each part's NULL fact for the entries lane, first seen (0 unseen, `NULLS_NONE`,
    /// `NULLS_SOME`, `NULLS_UNKNOWN`: no stats record, the part walks its rows).
    nulls_seen: Vec<u8>,
    runs: crate::ops::scan::pgrc::entry_runs::Runs,
    answers: Vec<Answer>,
    granules_answered: u64,
    parts_answered: u64,
    /// SCANSTATS: the position of this bind's statement entry (`pgrc_am::scanstats::granule_scan_start` / `gravity_slot`).
    ss_at: u32,
    /// The census counts not yet in the process statics.
    tally: Tally,
    bound: bool,
    cur: Cursor,
    /// The table lanes' once-cells and bump region (one part's tables at a time: reset at every part change).
    tables: TableState,
    /// The decode scratch of a pass over a verbatim part's rows (the ranks build, a table over them).
    scratch: crate::dict::text_hosts::TextScratch,
    /// D3038: an octet lane's granule lengths (`decode_octet`).
    octet_lens: Vec<u32>, // alloc: Node
    /// D3038 (R2): whose lengths `octet_lens` holds, `(part, column, granule, all valid)`, and their validity: the octet
    /// and emptiness lanes of one column read one decode a granule. Cleared at every bind.
    octet_memo: Option<(usize, i16, u32, bool)>,
    octet_bits: [u64; VALIDITY_WORDS],
    /// R1: a stored octet-length lane's granule words (`decode_octet`).
    octet_words: Vec<i32>, // alloc: Node
    /// R3: a character lane's granule lengths (`char_lengths`), and per `p * natts + k` its cursor over the column's own
    /// stream where the lane reads the stored octet lane (opened at its first multibyte granule).
    char_lens: Vec<u32>, // alloc: Node
    char_src: Vec<Option<Box<StreamCursor<'static>>>>, // alloc: Node
    /// D4464: the boxed cursors' own bytes (`cursors` and `char_src`, `CURSOR_BYTES` each; a cursor charges its tables
    /// itself), in `held_bytes`: counted at each open and close.
    cursors_held: usize,
    /// D4554 (the `kept_pool` switch): the kept cursors' own tables on one charge, moved once at each owner change
    /// (`pgrc_read::KeptPool`); `None` off, or before the first cursor opens.
    kept_pool: Option<Arc<pgrc_read::KeptPool>>,
    /// The render's images (`dict_value` over a verbatim part's code): bump chunks that never move for the statement.
    render: Vec<Vec<u64>>,
    render_used: usize,
    /// Per code lane, its renders over the current part (a fold renders every row; text_hosts `RenderMemo`).
    renders: Vec<crate::dict::text_hosts::RenderMemo>,
    /// X5: the site's key arrays `seek` was handed (address of the words, of the null bytes, the count): a bound code set's
    /// values are in its cells, written by the program at each window's bind before the lane fill (`bound_spec`).
    bound_keys: (usize, usize, usize),
}

const RENDER_CHUNK_MIN: usize = gravity_program::cost::pgrc::RENDER_CHUNK_MIN_WORDS;

/// D4464: what a parked source keeps of each bind-sized buffer (`park_scratch`), the planned output region's floor.
const SCRATCH_KEEP: usize = gravity_program::program::out_region_bytes(0, 0) as usize;

/// D4464: a boxed cursor's own bytes (`cursors_held`).
const CURSOR_BYTES: usize = core::mem::size_of::<StreamCursor<'static>>();

/// D4464: `v` freed when it holds more than `SCRATCH_KEEP` bytes.
fn shed<T>(v: &mut Vec<T>) {
    if gravity_driver::vec_bytes(v) > SCRATCH_KEEP {
        *v = Vec::new(); // alloc: Node
    }
}

/// D4554: `v` resized to `n` entries (`x` fills), the capacity it adds charged to `held` now (`kept_bytes` counts `v`, so
/// the next `sync` finds the bytes charged).
fn resize_charged<T: Clone>(v: &mut Vec<T>, n: usize, x: T, held: &mut HeldCharge) {
    let cap = v.capacity();
    v.resize(n, x); // alloc: Node charged held
    held.grow((v.capacity().saturating_sub(cap) * core::mem::size_of::<T>()) as u64);
}

/// D4554: `x` pushed onto `v`, the capacity it adds charged to `held` now (as `resize_charged`).
fn push_charged<T>(v: &mut Vec<T>, x: T, held: &mut HeldCharge) {
    let cap = v.capacity();
    v.push(x); // alloc: Node charged held
    held.grow((v.capacity().saturating_sub(cap) * core::mem::size_of::<T>()) as u64);
}

/// D3887 / D3902: the error a granule walk returns when it stopped for its statement's cancel (`crate::stash` reports it as the
/// cancel).
const WALK_CANCELED: &str = "gravity: a granule walk stopped for its statement's cancel";
const RENDER_CANCELED: &str = "gravity: a dictionary render stopped for its statement's cancel";

impl GranuleRangeSource {
    /// The recipe's registry index (derive's): each bind reads that entry's verdict cell from the statement's `Resolved`.
    pub fn at_site(mut self, site: usize) -> GranuleRangeSource {
        self.site = site;
        self
    }

    pub fn new(recipe: GranuleRecipe) -> GranuleRangeSource {
        GranuleRangeSource::homed(RecipeHome::Own(recipe))
    }

    /// sortmerge T2: a merged source's part stream source over the recipe it owns (`RecipeHome::Lent`).
    pub(crate) fn member(recipe: &'static GranuleRecipe) -> GranuleRangeSource {
        GranuleRangeSource::homed(RecipeHome::Lent(recipe))
    }

    /// A member's recipe lent again at its merged source's bind (where that source lives now).
    pub(crate) fn relend(&mut self, recipe: &'static GranuleRecipe) {
        if let RecipeHome::Lent(r) = &mut self.recipe {
            *r = recipe;
        }
    }

    fn homed(recipe: RecipeHome) -> GranuleRangeSource {
        let natts = recipe.natts;
        GranuleRangeSource {
            held: HeldCharge::new(),
            site: usize::MAX,
            cell: Arc::clone(&recipe.verdicts),
            recipe,
            resolved: core::ptr::null(),
            rel: 0,
            list: None,
            order: None,
            cursors: Vec::new(), // alloc: Node
            credit_from: 0,
            credit_part: usize::MAX,
            cursor_low: 0,
            seen_epoch: Vec::new(), // alloc: Node
            lanes_open: Vec::new(), // alloc: Node
            part_bind: Vec::new(), // alloc: Node
            binds: 0,
            held_parts: Vec::new(), // alloc: Node
            held_mark: Vec::new(), // alloc: Node
            touched: Vec::new(), // alloc: Node
            touch_live: false,
            park_bind: 0,
            bind_owner: None,
            park_pending: false,
            settle_bind: 0,
            slot_bound: false,
            rows_lane: 0,
            faces: Vec::new(), // alloc: Node
            cursor_generation: 0,
            cur_part: usize::MAX,
            dead: None,
            dead_memo: None,
            pin: None,
            stats_read: (usize::MAX, 0),
            datums: Vec::new(), // alloc: Node
            arenas: Vec::new(), // alloc: Node
            lane_byref: LaneBits::new(natts),
            inplace: vec![core::ptr::null(); natts], // alloc: Node
            valid: Vec::new(), // alloc: Node
            sel: Vec::new(), // alloc: Node
            sel16: Vec::new(), // alloc: Node
            sel16_ok: false,
            dense: Vec::new(), // alloc: Node
            mask: Box::new([0; VALIDITY_WORDS]), // alloc: Node
            selected: false,
            all_valid: vec![true; natts], // alloc: Node
            dv: Box::new([0; DV_GRANULE_WORDS]), // alloc: Node
            has_dv: false,
            lo: 0,
            hi: 0,
            g: 0,
            cut: None,
            cut_skip0: 0,
            staged: 0,
            nrows: 0,
            granules_decoded: 0,
            granules_skipped: 0,
            granules_selected: 0,
            zone: Vec::new(), // alloc: Exec
            nzone: 0,
            nzone_decl: 0,
            kept_mode: false,
            lens_mode: false,
            nzone_static: 0,
            has_bound: false,
            bound_ok: true,
            any: Vec::new(), // alloc: Exec
            any_ends: Vec::new(), // alloc: Exec
            any_base: 0,
            any_ok: false,
            tallied_mask: 0,
            tallied_win: false,
            nzone_bind: 0,
            zcode_images: Vec::new(), // alloc: Exec
            zcode_preds: Vec::new(), // alloc: Exec
            zcode_part: usize::MAX,
            zlen: Vec::new(), // alloc: Exec
            ne_codes: [None; ROW_PREDS],
            ne_placed: 0,
            ne_part: usize::MAX,
            part_dec_on: crate::inputs::part_decides_memo(),
            part_dec_p: usize::MAX,
            part_dec: Vec::new(), // alloc: Node
            key_zone: Vec::new(), // alloc: Exec
            key_vals: None,
            enables: VerdictCell::LEGACY,
            win: None,
            cond_mode: guc_tables::ConditionCacheMode::Off,
            cond_fp: [0, 0],
            cond_entry: None,
            cond_words: Box::new([0; VALIDITY_WORDS]), // alloc: Node
            cond_hit: 0,
            cond_mixed: false,
            cond_due: None,
            sparse_tab: None,
            sparse_dense: usize::MAX,
            table_dense: usize::MAX,
            rt: [RtSlot::NONE; RT_SLOTS],
            rt_first: 0,
            rt_gen: u64::MAX,
            late: 0,
            late_pend: 0,
            packed: 0,
            packed_win: 0,
            late_g: (0, 0, 0),
            late_done: Vec::new(),  // alloc: Node
            late_at: Vec::new(),    // alloc: Node
            late_more: Vec::new(),  // alloc: Node
            late_sel: Vec::new(),   // alloc: Node
            guard_sel: Vec::new(), // alloc: Node
            guard_late: 0,
            guard_null: 0,
            late_sel16: Vec::new(), // alloc: Node
            late_cnt: Vec::new(),   // alloc: Node
            late_req: Vec::new(),   // alloc: Node
            late_span: Vec::new(),  // alloc: Node
            late_calls: Vec::new(), // alloc: Node
            claim_runs: Vec::new(), // alloc: Node
            claim_miss_runs: Vec::new(), // alloc: Node
            unpin: false,
            unpin_at: (usize::MAX, u32::MAX),
            ahead: vec![u32::MAX; natts], // alloc: Node
            ahead_split: (false, false),
            ahead_runs: Vec::new(), // alloc: Node
            filter_lanes: LaneBits::new(natts),
            answer_lanes: LaneBits::new(natts),
            zone_lanes: LaneBits::new(natts),
            record_lanes: LaneBits::new(natts),
            ne_watch: 0,
            ne_elides: 0,
            ne_paid: 0,
            ne_asked: false,
            ne_dry: 0,
            mapped: LaneBits::new(natts),
            map_cols: vec![None; natts], // alloc: Node
            map_all: LaneBits::new(natts),
            map_all_part: usize::MAX,
            meta_need: LaneBits::new(natts),
            verdict: V_MIXED,
            lane_all: LaneBits::new(natts),
            elided: LaneBits::new(natts),
            first: LaneBits::new(natts),
            rest: LaneBits::new(natts),
            enc: Vec::new(), // alloc: Node
            enc_set: LaneBits::new(natts),
            sel_exact: false,
            enc_fell: false,
            sel_all: false,
            scratch_a: LaneBits::new(natts),
            scratch_b: LaneBits::new(natts),
            answered: false,
            entries_staged: false,
            runs_ok: false,
            runs_seen: Vec::new(), // alloc: Node
            nulls_seen: Vec::new(), // alloc: Node
            runs: crate::ops::scan::pgrc::entry_runs::Runs::default(),
            answers: vec![Answer::NONE; natts], // alloc: Node
            granules_answered: 0,
            parts_answered: 0,
            ss_at: u32::MAX,
            tally: Tally::default(),
            bound: false,
            cur: Cursor::new(),
            tables: TableState::new(),
            scratch: crate::dict::text_hosts::TextScratch::new(),
            octet_lens: Vec::new(), // alloc: Node
            octet_memo: None,
            octet_bits: [0; VALIDITY_WORDS],
            octet_words: Vec::new(), // alloc: Node
            char_lens: Vec::new(), // alloc: Node
            char_src: Vec::new(), // alloc: Node
            cursors_held: 0,
            kept_pool: None,
            render: Vec::new(), // alloc: Node
            render_used: 0,
            renders: Vec::new(), // alloc: Node
            bound_keys: (0, 0, 0),
        }
    }

    pub fn granules_decoded(&self) -> u64 {
        self.granules_decoded
    }

    /// D4554 (tests): this source's part-verdict memo on or off (`inputs::part_decides_memo` at its construction).
    #[doc(hidden)]
    pub fn set_part_decides_memo(&mut self, on: bool) {
        self.part_dec_on = on;
        self.part_dec_p = usize::MAX;
    }

    /// Granules this execution decoded selectively: the predicates' lanes whole, the rest for the
    /// surviving rows only (`row_select`).
    pub fn granules_selected(&self) -> u64 {
        self.granules_selected
    }

    /// Granules this execution skipped on a `NoneMatch` verdict (no decode, no window).
    pub fn granules_skipped(&self) -> u64 {
        self.granules_skipped
    }

    /// Granules this execution answered from statistics (a window of `rows` and records, no decode).
    pub fn granules_answered(&self) -> u64 {
        self.granules_answered
    }

    /// Parts this execution answered whole from the part-grain record (one window per part).
    pub fn parts_answered(&self) -> u64 {
        self.parts_answered
    }

    /// Table lanes this source built (per part, per statement): the census' per-source count.
    pub fn tables_built(&self) -> u64 {
        self.tables.built
    }

    /// Registry opens the statement performed over this source's relation (a resident part adds none).
    pub fn parts_opened(&self) -> u64 {
        if !self.bound {
            return 0;
        }
        self.rel_unbound().parts_opened()
    }

    /// This source's relation in the statement's `Resolved`.
    fn parts(&self) -> &RelParts {
        assert!(self.bound, "gravity: granule source read while unbound");
        self.rel_unbound()
    }

    pub fn recipe(&self) -> &GranuleRecipe {
        &self.recipe
    }

    pub fn total_granules(&self) -> u64 {
        self.list.as_ref().map_or(0, |l| l.granules())
    }

    fn bind_pg(&mut self, b: &BindingRef<'_>) -> PgResult<()> {
        // D3994 / D3204 (B.1 R): the kept cursors' part uses are the binding statement's (or portal's) while it runs, so
        // the watchdog's rank and the holder split see what it pins; `unbind_pg` gives them back to the session
        let (to, release) = memledger::charge::context();
        // D4554 (`kept_parked`): uses left pending on another owner go to the session first
        if self.park_pending && self.bind_owner != Some(to) {
            self.settle_pending();
        }
        let resolved = Resolved::of(b)?;
        let rel = resolved.index_of(self.recipe.rel).ok_or_else(|| err("gravity: granule source bound without a part list"))?;
        let rp = &resolved.rels[rel];
        let natts = self.recipe.natts;
        if self.recipe.attnos.iter().any(|&a| a < 0 || a as usize >= MAX_FACE_ATTNO) {
            return Err(err("gravity: granule source lane outside the face table"));
        }
        let list = Arc::clone(&rp.list);
        self.faces.clear();
        // bounded("key-columns: one face per scanned column, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..natts {
            let attno = self.recipe.attnos[k] as u32;
            let face = match self.recipe.shred_of(k) {
                Some(s) => list.faces.shred_face(attno, s).ok_or_else(|| err("gravity: more shred lanes than the list's face slots"))?,
                None => attno,
            };
            self.faces.push(face); // alloc: Node
        }
        self.rows_lane = (0..natts).find(|&k| self.recipe.shred_of(k).is_none()).unwrap_or(0);
        if list.generation() != self.cursor_generation {
            self.drop_cursors()?;
            self.cursor_generation = list.generation();
        }
        // D4315: whether this execution's cursors pass behind them (`unpin_window`): a releasing scan in granule order
        let unpin = crate::inputs::unpin_behind_enabled() && rp.stmt.is_some() && resolved.order_of(self.site).is_none() && self.recipe.sample.is_none();
        if mempolicy::keep::kept_parked() {
            self.bind_owner = Some(to);
            self.slot_bound = b.ordinal.is_some();
            // D4554: the kept cursors stay parked (no rejoin); a part's cursors take this execution's pass-behind and read
            // owner at its first open in the bind (`note_part`). A bind whose statement never unbound ends it first.
            // A part every claim of the statement left (D3268 / D3823's release of its use) is parked-only from then, so a
            // trim may evict it while the statement runs: the eviction takes no segment image another clone holds (D3113
            // `take_sole`), so a touched cursor's images stay in the part's cache, on the part cache's charge, until its
            // `park_images` at the unbind; none is on no owner.
            if self.touch_live {
                self.each_kept_of(KeptParts::Touched, |g, c| {
                    c.keep_past_statement_in(g);
                    c.set_pass_behind(false);
                });
            }
            self.touched.clear();
            self.touch_live = true;
        } else {
            self.each_kept_by_part(|g, c| {
                c.rejoin_in(g, to, release);
                c.set_pass_behind(unpin);
            });
        }
        // D4554: their pooled tables with them, once (a rejoin homes a cursor's tables on its statement or portal)
        if let Some(pool) = self.kept_pool.as_ref().filter(|_| matches!(to, memledger::Owner::Statement { .. } | memledger::Owner::Portal { .. })) {
            pool.home(Some((to, memledger::Kind::Region)));
        }
        self.binds += 1;
        if self.lanes_open.len() < list.len() {
            let n = list.len();
            self.lanes_open.resize(n, false); // alloc: Node
            resize_charged(&mut self.part_bind, n, 0, &mut self.held);
            resize_charged(&mut self.held_mark, n, false, &mut self.held);
            // cursors grow on the first `open_lanes`: a source answered whole never pays
        }
        // the statement's dictionaries are its `Resolved`'s: every part opens its lanes again (the kept cursors stay)
        self.lanes_open.fill(false);
        self.octet_memo = None;
        self.end_runs();
        if self.datums.len() < natts * (ROWS + PF_WORDS) {
            self.datums.resize(natts * (ROWS + PF_WORDS), 0); // alloc: Node
            self.arenas.resize_with(natts, Vec::new); // alloc: Node
            self.valid.resize(natts * VALIDITY_WORDS, 0); // alloc: Node
            self.sel.reserve(ROWS); // alloc: Node
            self.sel16.reserve(ROWS); // alloc: Node
            self.dense.resize(ROWS, 0); // alloc: Node
        }
        // D4328: a bind starts the render chunks over (chunk 0 rewritten, the rest freed); the release poison (debug builds,
        // PGRUST_GRAVITY_RELEASE_POISON=1) fills the images a render made before it with 0xDB, so a datum a consumer kept
        // into them past the bind (a worker slot binds again at the next section of a statement) reads 0xDB bytes
        if (self.render_used > 0 || self.render.len() > 1) && crate::inputs::release_poison() {
            // bounded("other: the render chunks the source's last binding filled, freed or rewritten by this bind anyway")
            for c in self.render.iter_mut() {
                c.fill(0xDBDB_DBDB_DBDB_DBDB);
            }
        }
        self.render.truncate(1);
        self.render_used = 0;
        self.renders.clear();
        self.resolved = resolved as *const Resolved;
        self.rel = rel;
        self.order = resolved.order_of(self.site);
        self.unpin = unpin;
        self.unpin_at = (usize::MAX, u32::MAX);
        self.list = Some(list);
        self.cur_part = usize::MAX;
        self.cursor_low = 0;
        self.seen_epoch.clear();
        self.pin = None;
        self.stats_read = (usize::MAX, 0);
        self.nrows = 0;
        self.lo = 0;
        self.hi = 0;
        self.g = 0;
        self.granules_decoded = 0;
        self.granules_skipped = 0;
        self.granules_answered = 0;
        self.parts_answered = 0;
        self.cur.clear();
        // D300: this execution's cell (a parked Node's recipe clone holds the admission's that built it)
        self.cell = Arc::clone(resolved.cell(self.site).unwrap_or(&self.recipe.verdicts));
        let (enables, fp) = self.cell.get();
        self.cond_mode = resolved.condition_cache_mode;
        self.enables = enables;
        if enables != VerdictCell::LEGACY {
            if self.cond_mode != guc_tables::ConditionCacheMode::Full {
                self.enables &= !VERDICT_UNREAD;
            }
            if self.cond_mode == guc_tables::ConditionCacheMode::Off {
                self.enables &= !VERDICT_COND;
            }
        }
        // D289: the word-valued predicates src.verdicts resolved for this execution follow the static ones (a generic
        // plan's `lane op $n` prunes as its custom plan's literal does); a shred lane's too, over its path's own records
        self.zone.truncate(self.nzone_static);
        self.key_zone.clear();
        self.key_vals = None;
        // NEW-72: a bound static predicate takes this execution's value; without one (no protocol, a contested cell) the
        // site takes no zone verdict in this execution: every window decodes and its filter runs per row
        self.bound_ok = !self.has_bound || bind_statics(&mut self.zone[..], &self.cell.bound()).is_some();
        if !self.any.is_empty() {
            // D3009: the disjunctions' bound values follow the static predicates' slots
            let base = self.any_base;
            let bound: Vec<(u16, i64)> = self.cell.bound().iter().filter(|&&(at, _)| at as usize >= base).map(|&(at, v)| ((at as usize - base) as u16, v)).collect(); // alloc: Exec
            self.any_ok = bind_statics(&mut self.any[..], &bound).is_some();
        }
        if self.on(VERDICT_ZONE) && self.bound_ok {
            // never one a static predicate already is; the cell's vector holds each word-valued one once (`add_word_zone`),
            // so this is linear in it: NEW-72's IN list of 1000 members is 1000 predicates at every worker's bind, under
            // the cell's lock (a scan of the growing list per member cost 5 ms at W8)
            let ns = self.nzone_static;
            let wz = self.cell.word_zone();
            self.zone.reserve(wz.len()); // alloc: Exec
            // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
            for z in wz.iter() {
                if (z.lane as usize) < natts && !self.zone[..ns].contains(z) {
                    self.zone.push(*z); // alloc: Exec
                }
            }
            drop(wz);
            // PS1-05: the open-time ones are read at each seek (`key_zone_seek`), after these
            self.key_zone.extend(self.cell.key_zone().iter().filter(|z| (z.lane as usize) < natts)); // alloc: Exec
        }
        // sortpath: the text constants the parts' code zones place (read at each part's pin: `pin_codes`)
        self.zcode_images.clear();
        self.zcode_part = usize::MAX;
        self.ne_part = usize::MAX;
        self.part_dec_p = usize::MAX;
        if self.on(VERDICT_ZONE) && self.bound_ok {
            self.zcode_images.extend(self.cell.code_images().iter().filter(|c| (c.lane as usize) < natts).cloned()); // alloc: Exec
        }
        let n = self.zone.len();
        let mut zlen = core::mem::take(&mut self.zlen);
        resize_charged(&mut zlen, n, NO_LEN, &mut self.held);
        // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
        for (l, z) in zlen.iter_mut().zip(&self.zone[..n]) {
            *l = self.const_len(z);
        }
        self.zlen = zlen;
        self.nzone_bind = n;
        if n > self.nzone_static || self.nzone_decl > self.nzone_static {
            self.zone_lanes.assign(&self.answer_lanes);
            // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
            for z in self.zone[..n].iter() {
                self.zone_lanes.set(z.lane as usize);
            }
            self.any_lanes();
        } else {
            self.arm_record_lanes();
        }
        self.nzone_decl = if self.bound_ok { n } else { 0 };
        self.nzone = if self.zone_on() && self.bound_ok { n } else { 0 };
        self.kept_mode = false;
        self.lens_mode = false;
        self.kept_mode = self.kept_mode_of();
        self.lens_mode = self.lens_mode_of();
        self.cond_fp = if self.on(VERDICT_COND) && (self.recipe.cond_ok || self.recipe.cond_unlogged) && self.recipe.verdict_lane.is_some() { fp } else { [0, 0] };
        self.cond_entry = None;
        self.cond_due = None;
        self.sparse_tab = None;
        self.sparse_dense = usize::MAX;
        self.table_dense = usize::MAX;
        // D2856 (colskip): each slot's window this execution's earlier sources of the site left (a rerun member run as a
        // section binds again at every run): a window spans the runs, never restarts per run
        for k in 0..RT_SLOTS {
            self.rt[k] = RtSlot::from_window(self.cell.rt_window_at(k));
        }
        self.rt_first = 0;
        self.rt_gen = u64::MAX;
        let lanes = if natts >= 64 { u64::MAX } else { (1u64 << natts) - 1 };
        // a shred lane decodes with the window, never late or packed: a part that did not elect its path has no cursor
        let lanes = self.recipe.shred.iter().filter(|s| s.lane < 64).fold(lanes, |m, s| m & !(1u64 << s.lane));
        self.late = if self.on(VERDICT_LATE) { self.cell.late() & lanes } else { 0 };
        self.guard_late = self.recipe.late_guards.iter().filter(|g| g.0 < 64).fold(0u64, |m, g| m | 1 << g.0) & self.late;
        self.guard_null = self.recipe.shred.iter().filter(|s| s.lane < 64 && !s.presence && !s.octet && null_when_absent(&s.path)).fold(0u64, |m, s| m | 1 << s.lane);
        self.late_pend = 0;
        let fits = natts * (ROWS + PF_WORDS) * 8 <= u32::MAX as usize;
        self.packed = if self.on(VERDICT_PACKED) && fits { self.cell.packed() & lanes } else { 0 };
        self.arm_map();
        self.packed_win = 0;
        if (self.late | self.packed) != 0 && self.late_at.len() < natts {
            self.late_done.resize(natts * VALIDITY_WORDS, 0); // alloc: Node
            self.late_at.resize(natts, (0, 0)); // alloc: Node
            self.late_more.resize_with(natts, Vec::new); // alloc: Node
            self.late_cnt.resize(natts, 0); // alloc: Node
            self.late_req.resize(natts, 0); // alloc: Node
            self.late_span.resize(natts, 0); // alloc: Node
            self.late_calls.resize(natts, (0, 0)); // alloc: Node
            self.late_sel.reserve(ROWS); // alloc: Node
            self.late_sel16.reserve(ROWS); // alloc: Node
        }
        self.win = None;
        self.dead_memo = None;
        self.bound = true;
        // SCANSTATS: the leader's bind is the scan (heap_source's rule), key-filtered when a range or equality zone predicate
        // in force is on the leading sorted_by key's lane; the list's totals once per scan (plain adds: landed at the
        // statement's end). Every bind keeps its statement entry's position for its unbind.
        if let Some(l) = self.list.as_deref() {
            self.ss_at = match b.ordinal {
                None => {
                    let key = self.recipe.key_lane;
                    let keyed = key.is_some() && self.zone[..self.nzone].iter().any(|z| Some(z.lane) == key && matches!(z.op, ZoneOp::Lt | ZoneOp::Le | ZoneOp::Eq | ZoneOp::Ge | ZoneOp::Gt));
                    pgrc_am::scanstats::granule_scan_start(self.recipe.rel, keyed, l.rows(), l.granules(), l.deleted())
                }
                Some(_) => pgrc_am::scanstats::gravity_slot(self.recipe.rel),
            };
        }
        Ok(())
    }

    /// Whether this execution takes verdict `bit` (`VERDICT_*`): never without a `src.verdicts` protocol.
    fn on(&self, bit: u64) -> bool {
        self.enables != VerdictCell::LEGACY && self.enables & bit != 0
    }

    /// P5-03 / H06: the code verdict's site table (`VERDICT_CODE_TABLE`) and its code lane.
    fn code_table(&self) -> Option<(usize, usize)> {
        let t = code_table(self.enables).filter(|_| self.on(VERDICT_CODE_TABLE))?;
        let k = self.recipe.tables.get(t)?.over as usize;
        (k < self.recipe.natts && self.recipe.kinds[k] == lane::KIND_CODES).then_some((t, k))
    }

    /// P5-03: part `p`'s code verdict rejects every code of its dictionary (no row of the part passes the leading code
    /// qual: a NULL row fails it, the qual being strict). The table is the part's (`sparse_table`, kept for the part); a
    /// counting execution (D1536) never takes it.
    fn code_part_none(&mut self, p: usize) -> PgResult<bool> {
        let Some((t, k)) = self.code_table().filter(|_| self.recipe.skipped.is_none()) else { return Ok(false) };
        if self.parts().list.faces.absent(self.face(k), p) {
            return Ok(false);
        }
        self.open_dict(p, k)?;
        if !matches!(self.dict_at(p, k).map(|e| &e.dict), Some(Dict::Part(_))) {
            return Ok(false);
        }
        Ok(self.sparse_table(p, t)?.and_then(|v| v.bits()).is_some_and(|b| b.rejects_all()))
    }

    /// PS1-05 (road `nest.eagersafe`): this seek's open-time zone predicates (`key_zone`: an eager-safe InitPlan's value the
    /// source's open stored in its key cell) after the bind's, exactly as bind adds D289's word-valued ones (the zone
    /// lanes, the mapped lanes). A seek whose values match the last one's changes nothing; one whose values moved (a
    /// re-open after the InitPlan ran again for a new outer row) also drops the serial census memo and the part statistics
    /// read under the previous ones.
    fn key_zone_seek(&mut self, keys: &[Word], keys_null: &[u8]) {
        if self.key_zone.is_empty() {
            return;
        }
        let vals: Vec<Option<ZonePred>> = self.key_zone.iter().map(|z| z.pred(keys, keys_null)).collect(); // alloc: Exec
        if self.key_vals.as_ref() == Some(&vals) {
            return;
        }
        self.zone.truncate(self.nzone_bind);
        self.part_dec_p = usize::MAX;
        // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
        for z in vals.iter().flatten() {
            if !self.zone.contains(z) {
                self.zone.push(*z); // alloc: Exec
            }
        }
        let n = self.zone.len();
        self.zone_lanes.assign(&self.answer_lanes);
        // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
        for z in self.zone[..n].iter() {
            self.zone_lanes.set(z.lane as usize);
        }
        self.any_lanes();
        self.nzone_decl = if self.bound_ok { n } else { 0 };
        self.nzone = if self.zone_on() && self.bound_ok { n } else { 0 };
        self.arm_map();
        self.dead_memo = None;
        self.stats_read = (usize::MAX, 0);
        self.key_vals = Some(vals);
    }

    /// A tick per cursor dropped; a cancel leaves the rest to the next bind (the source keeps the old generation).
    fn drop_cursors(&mut self) -> PgResult<()> {
        self.pin = None;
        let mut step = cancelpoll::step("granule_source bind: the old list's cursors dropped");
        for c in &mut self.cursors {
            step.tick().map_err(|_| err(WALK_CANCELED))?;
            if c.take().is_some() {
                self.cursors_held -= CURSOR_BYTES;
            }
        }
        self.cursors_held -= self.char_src.iter().flatten().count() * CURSOR_BYTES;
        self.char_src.clear();
        self.lanes_open.fill(false);
        self.clear_held();
        Ok(())
    }

    /// The granule records of part `p`'s zone lanes: read from the part when the catalog's
    /// part-grain record was all the statement held (a declared first touch, once per part).
    /// stage asks once per granule: the check stays inline, the read (once per part and runtime lane) is out of line
    #[inline(always)]
    fn granule_stats(&mut self, p: usize) -> PgResult<()> {
        // the bodies stay in the bound list's faces: read once per part (and runtime lane), not asked again per granule
        let rt = self.rt_mask();
        if self.stats_read == (p, rt) {
            return Ok(());
        }
        self.granule_stats_read(p, rt)
    }

    #[inline(never)]
    fn granule_stats_read(&mut self, p: usize, rt: u8) -> PgResult<()> {
        // a tick per lane: each may read its Stats body
        let mut step = cancelpoll::step("granule_source stage: a part's stats bodies");
        let rp = self.parts();
        // borrowed, not cloned: stage asks once per granule
        let part = rp.part_if_open(p).expect("pinned");
        for k in 0..self.recipe.natts {
            step.tick().map_err(|_| err(WALK_CANCELED))?;
            if self.zone_lanes.has(k) && !self.mapped.has(k) && !self.record_lanes.has(k) || self.rt_measuring_lane(k) {
                rp.body(p, self.face(k), part)?;
            }
        }
        self.stats_read = (p, rt);
        Ok(())
    }

    fn record_of(&self, p: usize, k: usize, lg: u32) -> Option<StatsRecord> {
        let body = self.parts().body_if_read(p, self.face(k))?;
        let off = lg as usize * STATS_RECORD_LEN;
        StatsRecord::decode(&mut Cur::new(body.get(off..off + STATS_RECORD_LEN)?)).ok()
    }

    /// `record_of`'s zone facts alone (`ZoneFacts`: no whole decode).
    #[inline(always)]
    fn facts_of(&self, p: usize, k: usize, lg: u32) -> Option<ZoneFacts> {
        let body = self.parts().body_if_read(p, self.face(k))?;
        ZoneFacts::of(body.get(lg as usize * STATS_RECORD_LEN..)?)
    }

    /// V4R-19 (A): the declared key lane when every row of the staged window holds one non-NULL key: its part's exact
    /// Stats record (the catalog's) has min == max and no NULL, else, in a part sorted on the key (equal keys
    /// together), its granule's does (the lane's body, read once per part as a granule walk's: an unsorted part's
    /// granules are rarely constant, e.g. a high-cardinality id); never under a delete mask, in an entries window or on an elided or
    /// by-reference lane. The key's datum (a signed word's exact key is the word) goes to the lane's element 0 at its
    /// width: the value every row holds, readable at the window's start though a late lane is not decoded yet.
    fn const_key_window(&mut self) -> PgResult<Option<usize>> {
        let k = const_key_lane(self.enables);
        if self.has_dv || self.entries_staged || k >= self.recipe.natts || self.elided.has(k) || self.lane_byref.has(k) || self.recipe.kinds[k] != lane::KIND_BYVAL {
            return Ok(None);
        }
        let list = self.list.as_ref().expect("bound");
        let (p, attno, face) = (self.cur_part, self.recipe.attnos[k] as u32, self.face(k));
        let (lg, prows) = ((self.staged - list.starts()[p]) as u32, list.parts()[p].rows);
        let one = |f: ZoneFacts, rows: u64| f.key_kind == KeyKind::Exact.as_u8() && f.min_key == f.max_key && f.nonnull as u64 == rows;
        // the footer's zone map (in memory, per granule) vetoes before any Stats read: a window its map shows as two values
        // is not constant (a high-cardinality id: a pread per part for the record, or for the walk of a part recorded sorted, was
        // +1.5 % cold W0); without a map a record neither held nor catalogued is not read
        let zone = list.zones.get().filter(|_| !Faces::is_shred(face)).and_then(|z| z.iter().find(|(a, _)| *a == attno)).map(|(_, c)| c.ranges()).filter(|r| r.len() as u64 == list.granules());
        let single = zone.as_ref().map(|r| r[self.staged as usize].0 == r[self.staged as usize].1);
        if single == Some(false) || single.is_none() && list.faces.lane(face).stats[p].get().is_none() && list.face_record(p, face).is_none() {
            self.tally.const_key[0] += 1;
            return Ok(None);
        }
        let rp = self.parts();
        let part = rp.part_if_open(p).expect("pinned");
        rp.stats(p, face, part)?;
        let pf = self.part_facts_of(p, k);
        let mut f = pf.filter(|&f| one(f, prows));
        let sorted = |f: ZoneFacts| [Sortedness::Ascending, Sortedness::Descending, Sortedness::Constant].iter().any(|s| s.as_u8() == f.sorted);
        if f.is_none() && pf.is_some_and(sorted) {
            rp.body(p, face, part)?;
            f = self.facts_of(p, k, lg).filter(|&f| one(f, self.nrows as u64));
        }
        self.tally.const_key[f.is_some() as usize] += 1;
        let Some(f) = f else { return Ok(None) };
        let m = u64::MAX >> (64 - 8 * self.recipe.widths[k].clamp(1, 8) as u32);
        self.datums[k * ROWS] = self.datums[k * ROWS] & !m | f.min_key as u64 & m;
        Ok(Some(k))
    }

    /// `part_record_of`'s zone facts alone.
    fn part_facts_of(&self, p: usize, k: usize) -> Option<ZoneFacts> {
        let body = self.parts().part_face(p, self.face(k))?;
        let lg = (body.len() / STATS_RECORD_LEN).checked_sub(1)?;
        ZoneFacts::of(body.get(lg * STATS_RECORD_LEN..)?)
    }

    /// The part-grain rollup of lane `k` of part `p`: the Stats body's last record (spec §8.1:
    /// granule records, band records, then the part).
    fn part_record_of(&self, p: usize, k: usize) -> Option<StatsRecord> {
        let body = self.parts().part_face(p, self.face(k))?;
        let n = body.len() / STATS_RECORD_LEN;
        let lg = n.checked_sub(1)?;
        StatsRecord::decode(&mut Cur::new(body.get(lg * STATS_RECORD_LEN..(lg + 1) * STATS_RECORD_LEN)?)).ok()
    }

    /// Fill the answer records of the answer lanes for granule `lg` (`None`: the whole part `p`
    /// from its part-grain record); false when one of them has no exact answer (then the window
    /// decodes, or the part goes granule by granule). `nonnull` always answers; the sum needs the
    /// computed witness; min / max need exact keys.
    fn answer(&mut self, p: usize, lg: Option<u32>) -> bool {
        // bounded("key-columns: one answer record per answered column, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..self.recipe.natts {
            if !self.answer_lanes.has(k) {
                continue;
            }
            let rec = match lg {
                Some(lg) => self.record_of(p, k, lg),
                None => self.part_record_of(p, k),
            };
            let Some(rec) = rec else { return false };
            if rec.flags & STATSF_COMPUTED == 0 || rec.key_kind != KeyKind::Exact.as_u8() {
                return false;
            }
            // NEW-47: an unsigned word's key is zero-extended; its min / max go as the program reads its 4-byte lane, signed
            let word = |key: i64| if self.unsigned_key(k) { key as i32 as i64 } else { key };
            let (min, max) = (word(rec.min_key), word(rec.max_key));
            // a bool lane's record tallies its false rows (the record's `zero_count`, pgrc_meta `ZeroKind::BoolFalse`); no
            // other lane's tally is handed on: a comparison with a constant is answered from the zone verdict alone. A
            // one-byte lane with no profile may be a bool's: no exact answer then (the window decodes)
            let falses = match self.recipe.profiles.get(k) {
                Some(Some(pr)) if pr.zero == pgrc_meta::profile::ZeroKind::BoolFalse => rec.zero_count,
                Some(Some(_)) => 0,
                _ if self.recipe.widths.get(k) == Some(&1) => return false,
                _ => 0,
            };
            let a = &mut self.answers[k];
            *a = Answer { nonnull: rec.nonnull as u64, min, max, flags: ANSWER_MINMAX, false_count: falses, ..Answer::NONE };
            a.set_sum(rec.sum_i128);
        }
        true
    }

    /// SRC-21: a verdict `v` (with its `Mixed` predicates `mixed`) the answered form takes as `AllMatch`: `Mixed` on tallied
    /// predicates alone, every other one `AllMatch` (a tallied predicate's passing rows are its lane's record's tally).
    #[inline(always)]
    fn tallies(&self, v: u8, mixed: u64) -> bool {
        v == V_MIXED && self.tallied_mask != 0 && mixed & !self.tallied_mask == 0
    }

    /// D1536: `n` rows a zone verdict skipped into this execution's count (`GranuleRecipe::skipped`), when it keeps one.
    #[inline]
    fn count_skipped(&self, n: u64) {
        if let Some(c) = self.recipe.skipped.as_deref() {
            c.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// A claim at the first granule of part `p` that runs to the part's end, no walk order, no
    /// deletes: decide the whole part from its part-grain records — skip it on `NoneMatch`, answer it
    /// as ONE window on `AllMatch` when every lane is filter-only or answered and every answer is
    /// exact; false when the part goes granule by granule.
    fn stage_part(&mut self, list: &PartList, p: usize, unit: u64) -> PartStep {
        let spec = &list.parts()[p];
        if self.order.is_some() || unit != list.starts()[p] || self.hi < list.end_of(p) || spec.deletes.is_some() || spec.rows > u32::MAX as u64 || self.nzone == 0 {
            return PartStep::Granules;
        }
        let rows = spec.rows as u32;
        let granules = spec.granules as u64;
        let _step = cancelpoll::step("granule_source stage: a part's verdict from its records");
        let (v, mixed) = self.part_verdicts_of(p, rows);
        // SRC-21: a part `Mixed` on its tallied predicates alone is answered as an `AllMatch` one (`tallies`)
        let v = if self.tallies(v, mixed) { V_ALL } else { v };
        match v {
            V_NONE => {
                self.count_skipped(spec.rows);
                self.g = list.end_of(p);
                self.granules_skipped += granules;
                self.tally.skipped += granules;
                PartStep::Skipped
            }
            V_ALL => {
                // VERDICT_ELIDE_HIT: the zone does not decide the filter lanes' quals, so no part is answered whole
                if self.on(VERDICT_ELIDE_HIT) || !self.filter_lanes.union_covers(&self.answer_lanes, self.recipe.natts) || !self.answer(p, None) {
                    return PartStep::Granules;
                }
                self.verdict = if mixed == 0 { V_ALL } else { V_MIXED };
                let mut la = core::mem::take(&mut self.lane_all);
                self.lane_all_of(mixed, &mut la);
                self.lane_all = la;
                self.elided.assign(&self.filter_lanes);
                self.answered = true;
                self.nrows = rows as usize;
                self.has_dv = false;
                self.staged = list.starts()[p];
                self.g = list.end_of(p);
                self.granules_answered += granules;
                self.parts_answered += 1;
                self.tally.answered += granules;
                self.tally.parts_answered += 1;
                PartStep::Answered
            }
            _ => PartStep::Granules,
        }
    }

    /// The dictionary union (`GranuleRecipe::entries`): granule `lg` of a dictionary-coded part with
    /// no delete vector stands for its codes `lg·ROWS ..` (every sealed entry is referenced by a
    /// row: `pgrc_write::elect` filters unreferenced entries at seal); a verbatim part walks its rows.
    /// PS1-25 (`path_entries`): the entries lane a text path's code lane, a part whose path dictionary is sealed publishes
    /// its entries, the guarded jsonb column each entry's one-key document (`path_docs`); a verbatim path's part its rows.
    fn stage_entries(&mut self, list: &PartList, p: usize, unit: u64, g: u64) -> PgResult<PartStep> {
        let Some(k) = self.recipe.entries else { return Ok(PartStep::Granules) };
        if self.recipe.entries_every {
            return self.stage_entries_every(list, p, unit, g);
        }
        let spec = &list.parts()[p];
        let path = self.path_entries(k);
        if !(path || self.recipe.natts == 1 && k == 0) || !entries_safe(spec, self.resolved_unbound()) || self.order.is_some() {
            return Ok(PartStep::Granules);
        }
        if path && !matches!(self.dict_at(p, k).map(|e| &e.dict), Some(Dict::Part(_))) {
            return Ok(PartStep::Granules);
        }
        let lg = g - list.starts()[p];
        let Some(ncodes) = self.entry_count(p, k, lg == 0)? else { return Ok(PartStep::Granules) };
        // RD-dict.entries: a declarer that keeps NULL (`entries_nulls`) takes one NULL row after a part's entries when the
        // part's lane holds one (every row of a part with a NULL is not an entry, so `ncodes + 1` rows still fit its granules)
        let null = match self.recipe.entries_nulls {
            false => false,
            true => match self.part_nulls(p, k)? {
                NULLS_NONE => false,
                NULLS_SOME => true,
                _ => return Ok(PartStep::Granules),
            },
        };
        let total = ncodes + null as u64;
        let lo = lg * ROWS as u64;
        if lo >= total || (self.runs_ok && self.part_runs(p)?) {
            return Ok(self.entries_skipped(unit));
        }
        let n = (total - lo).min(ROWS as u64) as usize;
        let nv = (ncodes - lo.min(ncodes)).min(n as u64) as usize;
        // a path declarer (PS1-25, the count form) never keeps NULL: its window is entries only
        debug_assert!(!path || !null);
        if path && (null || !self.path_docs(p, k, lo, n)?) {
            return Ok(PartStep::Granules);
        }
        let slot = &mut self.datums[k * ROWS..k * ROWS + n];
        let codes = super::raw::words_as_u32_mut(slot, n);
        // D2981: a descending declarer's windows count the part's codes from its highest (`entries_desc`)
        let desc = self.recipe.entries_desc;
        // bounded("small-fixed: the codes of one entries window, at most GRANULE_ROWS (8192)")
        for (i, c) in codes.iter_mut().enumerate() {
            *c = match (i < nv, desc) {
                (false, _) => 0,
                (true, false) => (lo as usize + i) as u32,
                (true, true) => (ncodes - 1 - (lo + i as u64)) as u32,
            };
        }
        debug_assert!(codes[..nv].windows(2).all(|w| if desc { w[0] > w[1] } else { w[0] < w[1] }), "an entries window's codes run in the declarer's order (code.cut's early end)");
        self.all_valid[k] = nv == n;
        if nv < n {
            // the part's NULL row (`entries_nulls`): row `nv`, the window's last, invalid
            let v = &mut self.valid[k * VALIDITY_WORDS..(k + 1) * VALIDITY_WORDS];
            v.fill(0);
            v[..nv / 64].fill(u64::MAX);
            if nv % 64 != 0 {
                v[nv / 64] = (1u64 << (nv % 64)) - 1;
            }
        }
        Ok(self.entries_window(n, unit, g))
    }

    /// PS1-25: entries lane `k` is a text path's code lane (`with_shred_code_lane`) and the recipe's one other lane its
    /// jsonb column (the lane the path guards: `with_late_guard`).
    fn path_entries(&self, k: usize) -> bool {
        self.recipe.natts == 2 && k < 2 && self.recipe.shred_of(k).is_some() && self.recipe.late_guards.contains(&((1 - k) as u8, k as u8))
    }

    /// PS1-25: the jsonb column's cells of an entries window over path lane `k`'s sealed dictionary in part `p` (codes
    /// `lo ..`, `n` of them): in row `i`, the one-key document {key: entry lo + i} in jsonb's binary form (adt_jsonb build.rs
    /// `convert_object` over one string pair), so the declarer's `col ->> 'key'` reads that entry as over a row holding it.
    /// A path past one root key: `false` (the rows).
    fn path_docs(&mut self, p: usize, k: usize, lo: u64, n: usize) -> PgResult<bool> {
        const JB_FOBJECT: u32 = 0x2000_0000;
        const JENTRY_HAS_OFF: u32 = 0x8000_0000;
        let c = 1 - k;
        let path = Arc::clone(&self.recipe.shred_of(k).expect("a path lane").path);
        let key = |out: &mut [u8]| {
            let (mut at, mut esc) = (0usize, false);
            // bounded("other: the bytes of the one jsonb path the plan names: the query text bounds it, as PG's jsonb key lookup reads the same key with no poll")
            for b in path.bytes() {
                if !esc && b == b'\\' {
                    esc = true;
                    continue;
                }
                if let Some(o) = out.get_mut(at) {
                    *o = b;
                }
                (at, esc) = (at + 1, false);
            }
            at
        };
        if path.bytes().fold((false, false), |(esc, dot), b| (!esc && b == b'\\', dot || !esc && b == b'.')).1 {
            return Ok(false);
        }
        let klen = key(&mut []);
        let Some(Dict::Part(d)) = self.dict_at(p, k).map(|e| &e.dict) else { return Ok(false) };
        let doc = |v: usize| (16 + klen + v).next_multiple_of(8);
        // a tick per entry sized and per entry copied (an entry's bytes are a value's)
        let mut step = cancelpoll::step("granule_source entries window: a path lane's documents");
        let mut total = 8;
        for i in 0..n {
            step.tick().map_err(|_| err(WALK_CANCELED))?;
            total += doc(d.entry((lo as usize + i) as u32).map_err(read_err)?.bytes.len());
        }
        let arena = &mut self.arenas[c];
        if arena.len() < total {
            arena.resize(total.next_power_of_two(), 0); // alloc: Node (kept at its high-water mark)
        }
        let base = arena.as_ptr() as usize;
        let mut at = base.next_multiple_of(8) - base;
        for i in 0..n {
            step.tick().map_err(|_| err(WALK_CANCELED))?;
            let e = d.entry((lo as usize + i) as u32).map_err(read_err)?;
            let (v, len) = (e.bytes, 16 + klen + e.bytes.len());
            let out = &mut arena[at..at + len];
            out[0..4].copy_from_slice(&((len as u32) << 2).to_ne_bytes());
            out[4..8].copy_from_slice(&(1 | JB_FOBJECT).to_ne_bytes());
            out[8..12].copy_from_slice(&(klen as u32 | JENTRY_HAS_OFF).to_ne_bytes());
            out[12..16].copy_from_slice(&(v.len() as u32).to_ne_bytes());
            key(&mut out[16..16 + klen]);
            out[16 + klen..].copy_from_slice(v);
            self.datums[c * ROWS + i] = (base + at) as u64;
            at += doc(v.len());
        }
        self.all_valid[c] = true;
        Ok(true)
    }

    /// Code lane `k`'s entry count in part `p`, `None` when its dictionary is not open (the part walks its rows); `first`:
    /// the part's first window, whose heads fault here (`head_prefault`).
    fn entry_count(&self, p: usize, k: usize, first: bool) -> PgResult<Option<u64>> {
        Ok(match self.dict_at(p, k) {
            Some(e) => Some(match &e.dict {
                Dict::Part(d) => {
                    // the first window's heads fault their block here, on the worker, not one part at a time on the leader
                    if first && self.recipe.head_prefault && d.ncodes() > 0 {
                        d.ensure_frame(0).map_err(read_err)?;
                    }
                    d.ncodes() as u64
                }
                Dict::Ranked { table, .. } => table.ranks().expect("ranks").ncodes() as u64,
            }),
            None => None,
        })
    }

    /// RD-dict.entries (`entries_nulls`): part `p`'s NULL fact for lane `k`'s column, read once per part from its stats
    /// records (each record's `nonnull` count is exact where the record exists: the COUNT answers read it); a part with a
    /// granule that has no record is `NULLS_UNKNOWN` and walks its rows.
    fn part_nulls(&mut self, p: usize, k: usize) -> PgResult<u8> {
        if self.nulls_seen.len() <= p {
            self.nulls_seen.resize(self.parts().list.len().max(p + 1), 0); // alloc: Node
        }
        if self.nulls_seen[p] == 0 {
            let face = self.face(k);
            let parts = self.parts();
            let fact = match parts.part_if_open(p) {
                // the face's granule-grain body, read once per list generation (`body`, the granule walk's)
                Some(part) => match parts.body(p, face, part)?.and_then(|b| part_has_null(part, b)) {
                    Some(true) => NULLS_SOME,
                    Some(false) => NULLS_NONE,
                    None => NULLS_UNKNOWN,
                },
                None => NULLS_UNKNOWN,
            };
            self.nulls_seen[p] = fact;
        }
        Ok(self.nulls_seen[p])
    }

    /// PS1-19 (`GranuleRecipe::entries_every`): granule `lg` of a dictionary-coded part with no delete vector stands, in
    /// each lane `j`, for lane `j`'s codes `lg·ROWS ..`, each code once (`stage_entries`' union, lane by lane); past lane
    /// `j`'s own count its rows are invalid, a NULL the declarer's `count(DISTINCT)` skips. The window has the most rows any
    /// lane has (every lane's count is at most the part's rows, so the part's granules hold them). A part with a lane
    /// whose dictionary is not open walks its rows, every granule alike.
    fn stage_entries_every(&mut self, list: &PartList, p: usize, unit: u64, g: u64) -> PgResult<PartStep> {
        let natts = self.recipe.natts;
        if !entries_safe(&list.parts()[p], self.resolved_unbound()) || self.order.is_some() {
            return Ok(PartStep::Granules);
        }
        let lg = g - list.starts()[p];
        let lo = lg * ROWS as u64;
        // a tick per lane: its entry count may fault its dictionary's first block
        let mut step = cancelpoll::step("granule_source entries window: every lane's entries");
        let mut most = 0u64;
        for j in 0..natts {
            step.tick().map_err(|_| err(WALK_CANCELED))?;
            let Some(nj) = self.entry_count(p, j, lg == 0)? else { return Ok(PartStep::Granules) };
            most = most.max(nj);
        }
        if lo >= most {
            return Ok(self.entries_skipped(unit));
        }
        let n = (most - lo).min(ROWS as u64) as usize;
        for j in 0..natts {
            step.tick().map_err(|_| err(WALK_CANCELED))?;
            let nj = self.entry_count(p, j, false)?.unwrap_or(0).saturating_sub(lo).min(n as u64) as usize;
            let codes = super::raw::words_as_u32_mut(&mut self.datums[j * ROWS..j * ROWS + n], n);
            // bounded("small-fixed: the codes of one entries window, at most GRANULE_ROWS (8192)")
            for (i, c) in codes.iter_mut().enumerate() {
                *c = if i < nj { (lo as usize + i) as u32 } else { 0 };
            }
            self.inplace[j] = core::ptr::null();
            self.all_valid[j] = nj == n;
            if nj < n {
                // rows `nj..n` invalid (never read: a NULL)
                let v = &mut self.valid[j * VALIDITY_WORDS..(j + 1) * VALIDITY_WORDS];
                v.fill(0);
                v[..nj / 64].fill(u64::MAX);
                if nj % 64 != 0 {
                    v[nj / 64] = (1u64 << (nj % 64)) - 1;
                }
            }
        }
        Ok(self.entries_window(n, unit, g))
    }

    /// An entries granule past every code of its part: skipped.
    fn entries_skipped(&mut self, unit: u64) -> PartStep {
        self.g = unit + 1;
        self.granules_skipped += 1;
        self.tally.skipped += 1;
        PartStep::Skipped
    }

    /// The entries window of `n` rows just staged at granule `g` (its lanes filled): a code window, never a granule's rows.
    fn entries_window(&mut self, n: usize, unit: u64, g: u64) -> PartStep {
        self.verdict = V_MIXED;
        // a code window, never a granule's rows: no condition-cache hit (a rows-path window's, left over, would publish its
        // row words as this window's selection), no PSMA window
        (self.cond_hit, self.cond_mixed, self.win) = (0, false, None);
        self.elided.clear();
        self.answered = false;
        self.has_dv = false;
        self.nrows = n;
        self.staged = g;
        self.g = unit + 1;
        self.entries_staged = true;
        self.granules_decoded += 1;
        self.tally.decoded += 1;
        PartStep::Answered
    }

    /// Road dict.entries' runs: part `p` (dictionary-coded, delete-free, its code lane's dictionary open) goes to the owner
    /// phase as its entry runs, taken whole the first time this source meets the part (later granules skip), or publishes
    /// its rows (runs unavailable: a faulted table). Every part's codes are in one or the other.
    fn part_runs(&mut self, p: usize) -> PgResult<bool> {
        if self.runs_seen.len() <= p {
            self.runs_seen.resize(self.parts().list.len().max(p + 1), 0); // alloc: Node
        }
        match self.runs_seen[p] {
            RUNS_TAKEN => return Ok(true),
            RUNS_ROWS => return Ok(false),
            _ => {}
        }
        // FASTADD: the entries lane's part (a stand-in for a column the part lacks)
        let part = self.lane_part(p, 0)?;
        let tkey = self.table_key(p, 0, &part)?;
        let read_err_cell = core::cell::Cell::new(None);
        let fp = {
            let Some(Dict::Part(d)) = self.dict_at(p, 0).map(|e| &e.dict) else { return Ok(false) };
            let view = crate::dict::text_hosts::DictView::Part(d, &read_err_cell);
            crate::dict::text_hosts::fp_table_in(self.fp_memo(), &part, tkey, &view, 0, false)
        };
        let runs = match fp {
            Ok(t) => crate::dict::text_hosts::entry_runs_in(self.fp_memo(), &part, tkey, &t),
            Err(crate::dict::text_hosts::TableBuildError::Pg(e)) => return Err(e),
            Err(crate::dict::text_hosts::TableBuildError::Read(e)) => return Err(read_err(e)),
            // the consumer's own table lane meets it again over the rows, where PG evaluates
            Err(crate::dict::text_hosts::TableBuildError::Region(_)) => None,
        };
        self.runs_seen[p] = if runs.is_some() { RUNS_TAKEN } else { RUNS_ROWS };
        if let Some(t) = runs {
            self.runs.push(t);
        }
        Ok(self.runs_seen[p] == RUNS_TAKEN)
    }

    /// The execution's runs dropped (their tables stay resident in the cache), the offer forgotten.
    fn end_runs(&mut self) {
        self.runs_ok = false;
        self.runs_seen.clear();
        self.nulls_seen.clear();
        self.runs.clear();
    }

    /// Pin part `p` for the claim: the statement's open part with its zone lanes' part-grain faces filled.
    /// D3902 (C9): `pin_part` (the part's lookup and its zone stats / meta reads) as its own labelled step, the walk's step
    /// ticked after it (under the cgroup's reclaim one part's open and decode took up to 4.3 s whole).
    fn pin_part_step(&mut self, walk: &mut Step, list: &PartList, p: usize) -> PgResult<()> {
        {
            let mut pin = cancelpoll::step("granule_source stage: a part's pin (lookup, zone stats and meta)");
            self.pin_part(&mut pin, list, p)?;
        }
        walk.tick().map_err(|_| err(WALK_CANCELED))
    }

    /// A tick per lane or predicate whose Stats or meta it may read.
    fn pin_part(&mut self, pin: &mut Step, list: &PartList, p: usize) -> PgResult<()> {
        self.ne_trial();
        let rp = self.parts();
        let part = rp.part(p, &self.recipe.registry)?;
        let meta = self.on(VERDICT_BLOOM) || self.on(VERDICT_PSMA);
        // the zone lanes' meta sections the faces lack in one read (cold: the 3 small preads per part become 1); warm, every
        // bloom / PSMA face is held and no section is looked up
        // card A: a mapped lane's sections are not read here: its verdicts are the map's, and its bloom / PSMA ride the claim
        // run for the granules the map leaves Mixed (late_meta_need); the runtime filter's lane still reads its body here
        let rt = |k: usize| self.rt_measuring_lane(k);
        let pinned = |k: usize| !self.mapped.has(k) && !self.record_lanes.has(k) || rt(k);
        if meta && self.zone[..self.nzone].iter().any(|z| pinned(z.lane as usize) && rp.meta_if_read(p, self.face(z.lane as usize)).is_none()) {
            let lanes: Vec<(u32, bool)> = (0..self.recipe.natts) // alloc: Cold
                .filter(|&k| self.zone_lanes.has(k) && pinned(k) || rt(k))
                .map(|k| (self.face(k), self.zone[..self.nzone].iter().any(|z| z.lane as usize == k)))
                .collect();
            rp.prefault_meta(p, &lanes, part, self.held.owner())?;
        }
        // D3155: the armed runtime filter's lane too, idle or measuring, when the catalog holds its part-grain record (no
        // part read): an idle sampled check reads it
        let rt_part = |k: usize| self.rt.iter().any(|s| s.lane == k && s.armed() && !s.text()) && list.face_record(p, self.face(k)).is_some();
        for k in 0..self.recipe.natts {
            pin.tick().map_err(|_| err(WALK_CANCELED))?;
            if self.zone_lanes.has(k) || rt_part(k) {
                rp.stats(p, self.face(k), part)?;
            }
        }
        if meta {
            for z in self.zone[..self.nzone].iter().filter(|z| pinned(z.lane as usize)) {
                pin.tick().map_err(|_| err(WALK_CANCELED))?;
                rp.meta(p, self.face(z.lane as usize), part)?;
            }
            // D3009: a disjunction's equality arms answer from their lanes' blooms
            if self.any_ok {
                for z in self.any.iter().filter(|z| z.op == ZoneOp::Eq && pinned(z.lane as usize)) {
                    pin.tick().map_err(|_| err(WALK_CANCELED))?;
                    rp.meta(p, self.face(z.lane as usize), part)?;
                }
            }
        }
        let part = Arc::clone(part);
        self.pin_codes(p, &part)?;
        let pin = PartPin::pin(&part);
        // card A: the mapped lanes this part proves NULL-free (an AllMatch from the map needs it)
        let mut all = core::mem::take(&mut self.map_all);
        all.clear();
        let rows = self.list.as_ref().map_or(0, |l| l.parts()[p].rows);
        // bounded("key-columns: one record lookup per mapped column, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..self.recipe.natts {
            if self.mapped.has(k) && self.part_facts_of(p, k).is_some_and(|f| f.nonnull as u64 == rows) {
                all.set(k);
            }
        }
        self.map_all = all;
        self.map_all_part = p;
        self.pin = Some(pin);
        Ok(())
    }

    /// Every zone and answer lane of part `p` has its part-grain face (the stats `pin_part` fills): `stage_part` decides
    /// the whole part from the list alone.
    fn faces_known(&self, list: &PartList, p: usize) -> bool {
        let known = |k: usize| list.faces.lane(self.face(k)).stats[p].get().is_some();
        (0..self.recipe.natts).all(|k| !(self.zone_lanes.has(k) || self.answer_lanes.has(k)) || known(k))
    }

    /// Open part `p`'s lane cursors the first time a claim enters it; its dictionaries open at the
    /// first decode or entries window (`open_dicts`), never for a part the verdicts skip whole.
    /// stage asks once per granule: the check stays inline, the opens (once per part) are out of line
    #[inline(always)]
    fn open_lanes(&mut self, p: usize) -> PgResult<()> {
        if self.lanes_open[p] {
            return Ok(());
        }
        self.open_lanes_cold(p)
    }

    #[inline(never)]
    fn open_lanes_cold(&mut self, p: usize) -> PgResult<()> {
        let mut step = cancelpoll::step("granule_source stage: a part's lanes opened");
        let part = Arc::clone(self.parts().part(p, &self.recipe.registry)?);
        let binding = pgrc_am::scan::codec_binding();
        let base = p * self.recipe.natts;
        // listed before any cursor of the part opens (an open that fails leaves the ones before it)
        self.note_part(p);
        self.grow_cursors();
        for k in 0..self.recipe.natts {
            if self.cursors[base + k].is_none() {
                // a shred lane the part did not elect has no cursor: `decode_lane` publishes it all-invalid (a code lane
                // with no dictionary and no codes, its tables `raw::publish_empty_table`: M6 R3)
                // FASTADD: a column the part lacks reads its stand-in
                let lane = Arc::clone(self.parts().lane(p, self.face(k), &part)?);
                let Some((attno, ord)) = self.parts().list.faces.stream_of(self.face(k), p, &lane)? else { continue };
                let mut cur = Box::new(StreamCursor::open(lane, binding, attno, ord).map_err(read_err)?); // alloc: Exec
                cur.set_pass_behind(self.unpin);
                self.pool_cursor(&mut cur);
                cur.set_owner(self.held.owner());
                self.cursors[base + k] = Some(cur);
                self.cursors_held += CURSOR_BYTES;
                // D3961: a lane's open reads its part's stream headers (a page fault each under reclaim): a tick per lane
                step.tick().map_err(|_| err(WALK_CANCELED))?;
            }
        }
        self.lanes_open[p] = true;
        self.note_part(p);
        Ok(())
    }

    /// D4554: part `p`'s cursors read in this bind (the `kept_decay` bound keeps them).
    #[inline]
    fn note_part(&mut self, p: usize) {
        let first = self.part_bind.get(p).is_some_and(|&b| b != self.binds);
        if let Some(b) = self.part_bind.get_mut(p) {
            *b = self.binds;
        }
        if first && self.touch_live {
            self.touch_part(p);
        }
        if self.held_mark.len() <= p {
            resize_charged(&mut self.held_mark, p + 1, false, &mut self.held);
        }
        if !self.held_mark[p] {
            self.held_mark[p] = true;
            push_charged(&mut self.held_parts, p as u32, &mut self.held);
        }
    }

    /// D4554 (`kept_parked`): part `p` opened in this bind: listed for the statement's end, and its kept cursors read for
    /// this execution (its pass-behind, the owner `held` is on), as the bind's walk set them with the switch off.
    #[cold]
    fn touch_part(&mut self, p: usize) {
        push_charged(&mut self.touched, p as u32, &mut self.held);
        let (natts, unpin, owner) = (self.recipe.natts.max(1), self.unpin, self.held.owner());
        let (lo, hi) = (p * natts, (p + 1) * natts);
        let (nc, ns) = (self.cursors.len(), self.char_src.len());
        // bounded("key-columns: one part's cursors, two per read column at most")
        for c in self.cursors[lo.min(nc)..hi.min(nc)].iter_mut().chain(self.char_src[lo.min(ns)..hi.min(ns)].iter_mut()).flatten() {
            c.set_pass_behind(unpin);
            c.set_owner(owner);
        }
    }

    /// D4554: whether this source holds a cursor (decode or character-length) over part `q`.
    fn holds_part(&self, q: usize) -> bool {
        let natts = self.recipe.natts.max(1);
        self.cursors.iter().skip(q * natts).take(natts).any(Option::is_some) || self.char_src.iter().skip(q * natts).take(natts).any(Option::is_some)
    }

    /// D4554: `held_parts` without the parts whose cursors all closed.
    fn prune_held(&mut self) {
        let natts = self.recipe.natts.max(1);
        let (cursors, chars, mark) = (&self.cursors, &self.char_src, &mut self.held_mark);
        self.held_parts.retain(|&q| {
            let q = q as usize;
            let open = cursors.iter().skip(q * natts).take(natts).any(Option::is_some) || chars.iter().skip(q * natts).take(natts).any(Option::is_some);
            if !open {
                if let Some(m) = mark.get_mut(q) {
                    *m = false;
                }
            }
            open
        });
    }

    /// D4554: `held_parts` emptied (every cursor dropped).
    fn clear_held(&mut self) {
        // bounded("other: one per part this source held cursors over")
        for &q in &self.held_parts {
            if let Some(m) = self.held_mark.get_mut(q as usize) {
                *m = false;
            }
        }
        self.held_parts.clear();
        self.touched.clear();
    }

    /// D4554 (the `kept_decay` bound, D binds): at the unbind, this source's cursors over a part none of its last D binds
    /// opened close (`close_cursors`): it keeps warm the parts its binds read again, and its kept set stops growing with
    /// its statements (a worker claims other parts each statement, and kept every cursor it ever opened).
    fn decay_cursors(&mut self) {
        if mempolicy::keep::kept_decay() == 0 {
            return;
        }
        // bounded("other: one check per part this source holds cursors over; it closes the stale ones")
        for i in 0..self.held_parts.len() {
            let q = self.held_parts[i] as usize;
            if self.part_bind.get(q).is_some_and(|&b| mempolicy::keep::kept_stale(self.binds.saturating_sub(b))) && self.holds_part(q) {
                self.close_cursors(q);
            }
        }
    }

    fn open_dicts(&mut self, p: usize) -> PgResult<()> {
        let mut step = cancelpoll::step("granule_source stage: a part's dictionaries opened");
        for k in 0..self.recipe.natts {
            if self.recipe.kinds[k] == lane::KIND_CODES && !self.parts().list.faces.absent(self.face(k), p) && self.dict_at(p, k).is_none() {
                self.open_dict(p, k)?;
                // D3961: a tick per dictionary opened
                step.tick().map_err(|_| err(WALK_CANCELED))?;
            }
        }
        Ok(())
    }

    fn grow_cursors(&mut self) {
        let n = self.lanes_open.len() * self.recipe.natts;
        if self.cursors.len() < n {
            self.cursors.resize_with(n, || None); // alloc: Node
        }
    }

    /// Code lane `k`'s dictionary of part `p` (the statement's); this source's cursor over the lane opens if it must.
    fn open_dict(&mut self, p: usize, k: usize) -> PgResult<()> {
        let j = self.face(k);
        // the open case first, with no Arc touched: every render asks
        if self.cursors.get(p * self.recipe.natts + k).is_some_and(|c| c.is_some()) && self.dict_at(p, k).is_some() {
            return Ok(());
        }
        let _step = cancelpoll::step("granule_source stage: a part's dictionary opened");
        let real = Arc::clone(self.parts().part(p, &self.recipe.registry)?);
        // FASTADD: a column the part lacks: its stand-in's cursor, dictionary and tables
        let part = Arc::clone(self.parts().lane(p, j, &real)?);
        self.note_part(p);
        self.grow_cursors();
        let base = p * self.recipe.natts;
        // this source's own cursor first: a render over a verbatim part decodes through it even when another source opened the dictionary
        if self.cursors[base + k].is_none() {
            let binding = pgrc_am::scan::codec_binding();
            let Some((attno, ord)) = self.parts().list.faces.locate(j, p, &part)? else { return Ok(()) };
            let mut cur = Box::new(StreamCursor::open(Arc::clone(&part), binding, attno, ord).map_err(read_err)?); // alloc: Exec
            cur.set_pass_behind(self.unpin);
            self.pool_cursor(&mut cur);
            cur.set_owner(self.held.owner());
            self.cursors[base + k] = Some(cur);
            self.cursors_held += CURSOR_BYTES;
        }
        let rp = self.rel_unbound();
        if self.dict_at(p, k).is_some() {
            return Ok(());
        }
        // D3038 (A): a length-only lane's verbatim part that sealed the octet-length lane codes it by length (kept.rs)
        if self.lens_mode && self.open_lens(p, k, &part)? {
            return Ok(());
        }
        // D3156: a kept-mode site decides the part's home first (kept.rs): its own kept table, else the list's cells below
        if self.kept_mode && self.open_kept(p, k, &real)? {
            return Ok(());
        }
        // a lane a table is built over walks every entry: dense (the whole stream once); the others serve point probes at block
        // grain — a lane whose only tables are its resident fingerprints, or tables whose builds read no entry
        // (`reads_no_entry`), walks nothing. A table that binary-searches the entries stays dense: its probes fault about
        // log2 of the payload's blocks, past the stream's break-even cold (`DictTouch`: two faults)
        let mut over = self.recipe.tables.iter().filter(|t| t.over as usize == k && !t.func.reads_no_entry()).map(|t| &t.func).peekable();
        let sparse = self.recipe.sparse_dicts
            && (over.peek().is_none()
                || (over.clone().all(|f| matches!(f, crate::dict::text_hosts::TableFn::Fingerprint | crate::dict::text_hosts::TableFn::FingerprintBpchar)) && {
                    let tkey = self.table_key(p, k, &part)?;
                    over.all(|f| crate::dict::text_hosts::fp_resident_as(&part, tkey, *f == crate::dict::text_hosts::TableFn::FingerprintBpchar).is_some())
                }));
        // card C: a dictionary not brought by a values run (the entries road reads no codes) reads its wrapped index
        // and, when walked, its payload in one pread, not one each
        let (run, hole) = read_tuning();
        let tail = self.dict_tail(p, k);
        // P5-18: a site that declares its probes (no renders) stays at block grain; a site that bounds its renders weighs
        // this part's share of them (by rows) against one stream of the part's own dictionary before it opens: where the
        // stream pays, the dictionary opens undeclared with its payload staged whole beside the index; else, and at a site
        // that cannot bound its renders, it opens for probes and the bytes-touched election re-checks as it faults
        let renders = self.recipe.dict_renders;
        let dense = sparse && renders != 0 && renders != u64::MAX && {
            let parts = self.parts().list.parts();
            let total = parts.iter().map(|s| s.rows as u128).sum::<u128>().max(1);
            let share = (renders as u128 * parts[p].rows as u128 / total).min(u64::MAX as u128) as u64;
            match self.parts().list.faces.locate(j, p, &part)? {
                Some((attno, ord)) => pgrc_read::wrapped_dense_pays(&part, attno, ord, share).map_err(read_err)?,
                None => false,
            }
        };
        if dense {
            fires::fire_shared("dict.open.whole"); // shapecheck fire signal (REGISTRY P5-18): a bound's renders opened a part's dictionary whole
        }
        let touch = (sparse && !dense).then_some(if renders == 0 { DictTouch::Sparse } else { DictTouch::Probe });
        // D3823: a lane whose renders read a table derived from its dictionaries keeps them in the list's cells
        let keep = self.renders_derived(k);
        // D4347: a verbatim part's ranks walk (`row_codes`) reads the column whole, and a lane with a table over it (a LIKE
        // table's granule scans) or a site that renders reads it again in the same claim: the cursor keeps what the walk
        // read pinned until the scan passes it by band (`StreamCursor::hold_for_walks`, `unpin_window`), so the second
        // read faults nothing the first evicted. A lane read once (its codes only) passes on load as before
        let again = renders != 0 || self.recipe.tables.iter().any(|t| t.over as usize == k);
        let cur = self.cursors[base + k].as_deref_mut().expect("cursor");
        if again {
            cur.hold_for_walks();
        }
        if run > 0 {
            cur.stage_dict(tail.payload && pgrc_read::wrapped_payload_whole(touch), run, hole);
        }
        rp.dict(p, j, &part, cur, &mut self.scratch, touch, keep)?;
        Ok(())
    }

    /// D3823: lane `k` renders through a table built from each part's dictionary (a derived key's values, a program's
    /// table): a uses-only release keeps its dictionaries in the list's cells, as before, since a render after the part's
    /// release would open the dictionary and build the table again (a `regexp_replace` key's emit opened 303 of them again).
    fn renders_derived(&self, k: usize) -> bool {
        use crate::dict::text_hosts::TableFn;
        self.recipe.tables.iter().any(|t| t.over as usize == k && matches!(t.func, TableFn::RegexpReplace { .. } | TableFn::DerivedFingerprint { .. } | TableFn::DerivedCall { .. } | TableFn::Program { .. }))
    }

    /// M2-ZONEREAD card C: whether code lane `k`'s dictionary payload in part `p` is walked entry by entry: a table whose
    /// function reads the entries' bytes, or the lane's fingerprints (the entries road's runs, a fingerprint table) when
    /// no resident Fp128 table of the part holds them. Its first entry faults the whole payload, so the payload
    /// is read beside the index. Never for a length table (the index's) or a lane only rendered or compared by code (a
    /// render may touch no row): those read as before.
    fn walks_payload(&self, p: usize, k: usize) -> bool {
        use crate::dict::text_hosts::TableFn;
        let over = || self.recipe.tables.iter().filter(move |t| t.over as usize == k);
        if over().any(|t| matches!(t.func, TableFn::Like { .. } | TableFn::LikeBound { .. } | TableFn::RankBpchar | TableFn::Canon | TableFn::CanonBpchar | TableFn::RegexpReplace { .. } | TableFn::DerivedFingerprint { .. } | TableFn::DerivedCall { .. } | TableFn::Program { .. })) {
            return true;
        }
        // the entries road reads the padded form (its runs); a bpchar key's trimmed form is its own table (D767)
        let padded = self.recipe.entries == Some(k) || over().any(|t| t.func == TableFn::Fingerprint);
        let bpchar = over().any(|t| t.func == TableFn::FingerprintBpchar);
        if !padded && !bpchar {
            return false;
        }
        // FASTADD: the lane's part (a stand-in for a column the part lacks)
        let Ok(part) = self.lane_part(p, k) else { return false };
        let Ok(tkey) = self.table_key(p, k, &part) else { return true };
        let missing = |b: bool| crate::dict::text_hosts::fp_resident_as(&part, tkey, b).is_none();
        (padded && missing(false)) || (bpchar && missing(true))
    }

    /// M2-ZONEREAD card C: the wrapped dictionary regions of code lane `k` in part `p` a read should bring along, raw,
    /// for the dictionary reads that follow (decode and the entries road open every code lane's dictionary; the handle's
    /// first reader of a region unwraps it from the stage): the index while no reader holds it (the dictionary not open,
    /// or opened ahead of the claim run with its index unread), the payload too when the lane walks it (`walks_payload`), the
    /// handle will unwrap it whole and the dictionary is not open. Any other lane, or an open dictionary whose index is
    /// read, brings none.
    fn dict_tail(&self, p: usize, k: usize) -> DictTail {
        if self.recipe.kinds[k] != lane::KIND_CODES {
            return DictTail::NONE;
        }
        match self.dict_at(p, k).map(|e| &e.dict) {
            None => DictTail { index: true, payload: pgrc_read::wrapped_payload_whole(None) && self.walks_payload(p, k) },
            Some(Dict::Part(d)) if !d.index_resident() => DictTail { index: true, payload: false },
            Some(_) => DictTail::NONE,
        }
    }

    /// Lane `k`'s face in the bound list (`faces`, set at bind): its column's attno, or its shred lane's slot.
    #[inline(always)]
    fn face(&self, k: usize) -> u32 {
        self.faces[k]
    }

    /// Granule `lg` of part `p`'s row count: the `rows_lane` cursor's, else (a recipe of shred lanes only, the part
    /// electing none of them; or the part's lanes not open) the part's own geometry.
    #[inline]
    fn granule_rows(&self, p: usize, lg: u32) -> u32 {
        match self.cursors.get(p * self.recipe.natts + self.rows_lane).and_then(|c| c.as_deref()) {
            Some(c) => c.rows_in_granule(lg),
            None => self.parts().part_if_open(p).map_or(0, |part| pgrc_format::geom::rows_in_granule_at(part.rows(), part.grain(), lg)),
        }
    }

    /// Whether lane `k` is a shred lane part `p` did not elect (no cursor: every row invalid).
    #[inline]
    fn absent(&self, p: usize, k: usize) -> bool {
        self.parts().list.faces.absent(self.face(k), p)
    }

    /// Lane `k`'s part-resident table key in part `p` (`stream_key`): a column's attno, a shred lane's path key.
    fn table_key(&self, p: usize, k: usize, part: &OpenPart) -> PgResult<u32> {
        let (attno, ord) = self.parts().list.faces.locate(self.face(k), p, part)?.ok_or_else(|| err("gravity: a table over a shred lane the part did not elect"))?;
        Ok(stream_key(attno, ord))
    }

    /// FASTADD: the part lane `k` of the pinned part `p` reads (`RelParts::lane`).
    fn lane_part(&self, p: usize, k: usize) -> PgResult<Arc<OpenPart>> {
        let rp = self.parts();
        let real = rp.part_if_open(p).ok_or_else(|| err("gravity: a lane's part not pinned"))?;
        Ok(Arc::clone(rp.lane(p, self.face(k), real)?))
    }

    /// One coalesced read per lane of the claim's granules of part `p` from `g` on (run knob on,
    /// granule-order claims only); lanes a part-grain `AllMatch` elides are not read, and the filter-only lanes a
    /// condition-cache hit elides (`stage`: a hit decides the window) only for the granules the cache does not know.
    fn read_claim_run(&mut self, list: &PartList, p: usize, g: u64) {
        let (run, hole) = read_tuning();
        if run == 0 || self.order.is_some() {
            return;
        }
        let spec = &list.parts()[p];
        let start = list.starts()[p];
        let (lo, hi) = (g.saturating_sub(start), self.hi.min(list.end_of(p)).saturating_sub(start));
        if lo >= hi || hi > u32::MAX as u64 {
            return;
        }
        // a tick per granule tested and per lane advised or read; a cancel ends each loop (the walk's next tick stops)
        let mut step = cancelpoll::step("granule_source stage: a claim run read");
        let mut la = core::mem::take(&mut self.scratch_a);
        let mut elide = core::mem::take(&mut self.scratch_b);
        if self.nzone > 0 && spec.rows <= u32::MAX as u64 {
            let (v, mixed) = self.part_verdicts_of(p, spec.rows as u32);
            self.lane_all_of(mixed, &mut la);
            self.lane_elision_into(v, false, &la, &mut elide);
        } else {
            elide.clear();
        }
        self.scratch_a = la;
        let base = p * self.recipe.natts;
        // read.claim (H03b): the runs of granules the verdicts admit, else the claim's whole range
        let mut runs = core::mem::take(&mut self.claim_runs);
        let mut miss = core::mem::take(&mut self.claim_miss_runs);
        runs.clear();
        miss.clear();
        let push = |v: &mut Vec<(u32, u32)>, lg: u32| match v.last_mut() {
            Some(r) if r.1 == lg => r.1 = lg + 1,
            _ => v.push((lg, lg + 1)), // alloc: Node (kept at its high-water mark)
        };
        // a hit elides the filter-only lanes (lane_elision_into under a decided window): theirs follow the misses only
        let claim = self.on(VERDICT_CLAIM) && self.cursors[base + self.rows_lane].is_some();
        // decode.unread: a hit reads no lane at all
        let unread = self.on(VERDICT_UNREAD);
        let split = claim && self.cond_fp != [0, 0] && (unread || (self.filter_lanes.any() && (self.enables == VerdictCell::LEGACY || self.enables & VERDICT_ELIDE != 0)));
        if claim {
            for lg in lo as u32..hi as u32 {
                if step.tick().is_err() {
                    break;
                }
                let rows = self.cursors[base + self.rows_lane].as_deref().expect("cursor").rows_in_granule(lg);
                if !self.admits(p, lg, rows) {
                    fires::fire_shared("pgrc.claim_hole"); // shapecheck fire signal (RD-read.claim): the run skips a refused granule
                    continue;
                }
                push(&mut runs, lg);
                // admits asked the cache (cond_ready): an entry for `p` is the part's
                if split && self.cond_entry.as_ref().is_none_or(|(q, e)| *q != p || matches!(e.get(lg as usize), crate::ops::scan::pgrc::cond_cache::Hit::Unknown)) {
                    push(&mut miss, lg);
                }
            }
        } else {
            runs.push((lo as u32, hi as u32));
        }
        // read.advise (PERF-COLD): every lane's non-resident extents of the claim go to the device's queue first, in calls
        // of the device's advice size, so the lanes' preads below overlap instead of running one at a time (cold, without it:
        // one request in flight per worker). A hot claim advises nothing. The advice held by claims whose reads have not
        // run yet stays within the device's queue (`advise_budget`), so concurrent claims do not queue ahead of each
        // other's demand reads (heap.stream's rule 4 for pgrc claims).
        let mut held = 0u64;
        // D4709: the claim's advice counts add to the process totals once, below (a claim that advised nothing adds nothing)
        let (mut advised_calls, mut advised_bytes) = (0u64, 0u64);
        if let Some(piece) = advise_piece() {
            let budget = advise_budget();
            for k in 0..self.recipe.natts {
                if step.tick().is_err() {
                    break;
                }
                if elide.has(k) || (k < 64 && self.guard_late >> k & 1 != 0) || self.lens_unread(p, k) {
                    continue;
                }
                let rs = if split && (unread || self.filter_lanes.has(k)) { &miss } else { &runs };
                let room = budget.saturating_sub(ADVISE_INFLIGHT.load(Ordering::Relaxed));
                if room == 0 {
                    break;
                }
                if let Some(cur) = self.cursors[base + k].as_deref().filter(|_| !rs.is_empty()) {
                    let (calls, bytes) = cur.advise_granule_runs(rs, hole, piece, room);
                    if bytes > 0 {
                        ADVISE_INFLIGHT.fetch_add(bytes, Ordering::Relaxed);
                        note_rmws(1);
                    }
                    held += bytes;
                    advised_calls += calls;
                    advised_bytes += bytes;
                }
            }
        }
        let _ = add_advised(advised_calls, advised_bytes);
        if self.unpin {
            self.ahead.fill(u32::MAX);
            self.ahead_split = (split, unread);
        }
        self.prefetch_lanes(&mut step, p, lo as u32, true, &runs, &miss, &elide, (split, unread), run, hole);
        // the claim's reads ran: its advice leaves the budget
        if held > 0 {
            ADVISE_INFLIGHT.fetch_sub(held, Ordering::Relaxed);
        }
        self.claim_runs = runs;
        self.claim_miss_runs = miss;
        self.scratch_b = elide;
    }

    /// The lanes' reads of a claim's runs in part `p` (`read_claim_run`): every run at once, or (D4315, `unpin`) each lane
    /// from granule `from` to its read-ahead end (`StreamCursor::read_ahead_end`: about a read run of its file bytes), the
    /// lanes whose windows reached their end only after the first call (`first`: the claim's entry, which also brings the
    /// dictionary tail and the meta sections).
    #[allow(clippy::too_many_arguments)]
    fn prefetch_lanes(&mut self, step: &mut cancelpoll::Step, p: usize, from: u32, first: bool, runs: &[(u32, u32)], miss: &[(u32, u32)], elide: &LaneBits, (split, unread): (bool, bool), run: u64, hole: u64) {
        let base = p * self.recipe.natts;
        // D4709: the call's read counts, added to the process totals once after the lanes (not five RMWs per lane)
        let mut pf = [0u64; 5];
        for k in 0..self.recipe.natts {
            if step.tick().is_err() {
                break;
            }
            // a late jsonb column its path lanes guard (M6 D2) is read where a path lane is invalid, not with the claim; a lane
            // coded by length (D3038 A, kept.rs `lens_unread`) reads none of its column's bytes
            if elide.has(k) || (k < 64 && self.guard_late >> k & 1 != 0) {
                continue;
            }
            if self.lens_unread(p, k) {
                fires::fire_shared("octet.lens_unread"); // shapecheck fire signal (D3038 A): a claim skips a length-coded lane's column stream
                continue;
            }
            let rs = if split && (unread || self.filter_lanes.has(k)) { miss } else { runs };
            // D4315: a lane reads ahead from `from` to its read-ahead end, again once its windows pass that end
            let mut clipped = core::mem::take(&mut self.ahead_runs);
            let rs = if self.unpin {
                if !first && (self.ahead[k] == u32::MAX || from < self.ahead[k]) {
                    self.ahead_runs = clipped;
                    continue;
                }
                let Some(cur) = self.cursors[base + k].as_deref() else {
                    self.ahead_runs = clipped;
                    continue;
                };
                // past what the lane's cursor already read: a whole-column walk read (and passed) the part at its first band,
                // and a read-ahead of it again would bring back images no reader loads, held unpassed until the claim left
                let from = if first { from } else { from.max(cur.read_hi()) };
                let end = cur.read_ahead_end(from, run);
                self.ahead[k] = end;
                clipped.clear();
                clipped.extend(rs.iter().map(|&(a, b)| (a.max(from), b.min(end))).filter(|r| r.0 < r.1)); // alloc: Node (kept at its high-water mark)
                &clipped[..]
            } else {
                rs
            };
            // the admitted runs in one run call: neighbouring runs' extents coalesce (a pread per run split a cold scan's reads)
            if let Some(cur) = self.cursors[base + k].as_deref().filter(|_| !rs.is_empty()) {
                // card C: a code lane's run also brings its wrapped dictionary regions the statement reads next; card A: a
                // mapped lane's run its Stats / Bloom / PSMA sections, which sit right after its streams
                let mut secs = [0usize; pgrc_read::cursor::TAIL_SECTIONS];
                let mut ns = 0;
                if first && self.meta_need.has(k) {
                    let attno = self.recipe.attnos[k] as u32;
                    if let Some(part) = self.parts().part_if_open(p) {
                        for kind in [SectionKind::Stats, SectionKind::Bloom, SectionKind::Psma] {
                            if let Some(i) = part.find_section(kind, attno, 0) {
                                secs[ns] = i;
                                ns += 1;
                            }
                        }
                    }
                }
                let tail = if first { self.dict_tail(p, k) } else { DictTail::NONE };
                let r = cur.prefetch_granule_runs_tail(rs, run, hole, tail, &secs[..ns]);
                pf[0] += r[0];
                pf[1] += r[1];
                pf[2] += r[2];
                pf[3] += r[3];
                pf[4] += r[4];
            }
            self.ahead_runs = clipped;
        }
        let _ = add_prefetched(pf);
    }

    /// D4315: the claim stages granule `lg` of part `p` (every window before it was consumed whole: `window_pg`). Its lanes'
    /// cursors pass each extent as they load the next (`StreamCursor::set_pass_behind`: the images stay cached, and a
    /// content trim past the cache's level may take them while the part stays in use); at each new band the trim runs, and
    /// a lane whose read-ahead `lg` reached reads its next run (`prefetch_lanes`). With memory to spare nothing is evicted:
    /// a hot repeat finds them.
    fn unpin_window(&mut self, p: usize, lg: u32, entered: bool) {
        let band = lg / pgrc_format::geom::GRANULES_PER_BAND;
        if self.unpin_at.0 != p {
            self.unpin_at = (p, u32::MAX);
            if !entered {
                self.ahead.fill(u32::MAX);
            }
        }
        if self.unpin_at.1 != band {
            // the cursors passed what they read before this band as they loaded it (`StreamCursor::set_pass_behind`); a
            // cursor whose column a walk read whole passes the extents before the band now (D4347: `hold_for_walks`)
            self.unpin_at.1 = band;
            let base = p * self.recipe.natts;
            // bounded("key-columns: one cursor per lane of the part, at most MaxTupleAttributeNumber (1664)")
            for k in 0..self.recipe.natts {
                if let Some(cur) = self.cursors.get_mut(base + k).and_then(|c| c.as_deref_mut()).filter(|c| c.walked()) {
                    cur.pass_behind(lg);
                }
            }
            self.recipe.registry.trim_if_over();
        }
        let (run, hole) = read_tuning();
        if entered || run == 0 || !self.ahead.iter().any(|&e| e != u32::MAX && lg >= e) {
            return;
        }
        let mut step = cancelpoll::step("granule_source stage: a lane's read-ahead");
        let (runs, miss, elide) = (core::mem::take(&mut self.claim_runs), core::mem::take(&mut self.claim_miss_runs), core::mem::take(&mut self.scratch_b));
        let split = self.ahead_split;
        self.prefetch_lanes(&mut step, p, lg, false, &runs, &miss, &elide, split, run, hole);
        self.claim_runs = runs;
        self.claim_miss_runs = miss;
        self.scratch_b = elide;
    }

    /// Stage the claim's next granule that is not `NoneMatch`; false at the claim's end.
    fn stage(&mut self) -> PgResult<bool> {
        // D3887 (C9): the walk past granules and parts its zones and caches skip opens parts and reads their maps, unbounded
        // per call: a tick per granule and after each part-grain step; it stops on its statement's cancel (`crate::stash`
        // reports it as the cancel)
        let mut step = cancelpoll::step("granule_source stage: the claim's walk to its next window");
        loop {
            step.tick().map_err(|_| err(WALK_CANCELED))?;
            // skewsplit (D3320): the granule step, before any part-grain step reads `hi`: an idle worker's ask moves the
            // claim's end to the midpoint of what is left (the back half is its claim now)
            if let Some(mid) = self.cut.and_then(|c| c.step(self.g, self.granules_skipped.saturating_sub(self.cut_skip0))) {
                self.hi = mid;
                fires::fire_shared("pgrc.claim_cut");
            }
            if self.g >= self.hi {
                self.pin = None;
                self.tally.flush();
                let q = core::mem::replace(&mut self.credit_part, usize::MAX);
                self.part_leave(&mut step, q);
                return Ok(false);
            }
            // a borrow, not an Arc::clone per granule: every worker's clone and drop hit the list's one refcount line
            let list: &PartList = self.list_unbound().ok_or_else(|| err("gravity: granule claim on an unbound source"))?;
            // a walk order maps the claim's unit to its granule
            let unit = self.g;
            self.entries_staged = false;
            self.selected = false;
            self.sel_all = false;
            self.inplace.fill(core::ptr::null());
            let g = match &self.order {
                Some(o) => *o.get(unit as usize).ok_or_else(|| err("gravity: walk unit outside the order"))? as u64,
                None => unit,
            };
            let p = if self.cur_part < list.len() && g >= list.starts()[self.cur_part] && g < list.end_of(self.cur_part) { self.cur_part } else { list.part_of(g) };
            if p != self.credit_part {
                // D2985: the claim leaves the part it was in (never the previous claim's part: a claim credits only its own)
                let q = core::mem::replace(&mut self.credit_part, p);
                self.part_leave(&mut step, q);
            }
            if p != self.cur_part {
                self.pin = None;
                self.part_enter(&mut step, p);
                self.cur_part = p;
            }
            let entered = self.pin.is_none();
            let mut late_pin = false;
            if entered {
                // the serial census: a part wholly inside a dead range (no granule a zone predicate keeps) is
                // jumped before it is armed, looked up or pinned, as the runner's census never claims it. D1536: a
                // counting execution jumps only a whole part (a dead part has no deletes: its rows are its count)
                if self.dead.as_deref().is_some_and(|d| dead_part(d, list.starts()[p], list.end_of(p))) && (self.recipe.skipped.is_none() || unit == list.starts()[p] && self.hi >= list.end_of(p)) {
                    let end = self.hi.min(list.end_of(p));
                    self.count_skipped(list.parts()[p].rows);
                    self.granules_skipped += end - unit;
                    self.tally.skipped += end - unit;
                    self.g = end;
                    continue;
                }
                // the runtime filter over the whole part from its catalog-grain record, before the part is opened
                self.rt_arm();
                if self.order.is_none() && self.rt_cut_any(p, None) {
                    let end = self.hi.min(list.end_of(p));
                    self.granules_skipped += end - unit;
                    self.tally.skipped += end - unit;
                    self.tally.rt_skipped += end - unit;
                    self.g = end;
                    continue;
                }
                // the serial answer (a table of many whole parts): a list whose part-grain faces are known (a warm scan)
                // answers or skips a whole part before its lookup and pin, which a part the records decide never reads
                late_pin = self.faces_known(list, p);
                if !late_pin {
                    self.pin_part_step(&mut step, &list, p)?;
                }
                // PS1-22: a text key's runtime filter over the whole part from its dictionary's fingerprints (pinned first)
                let mut text_cut = false;
                // bounded("small-fixed: the runtime filter slots, RT_SLOTS (4)")
                for i in 0..if self.order.is_none() { RT_SLOTS } else { 0 } {
                    let k = (self.rt_first + i) % RT_SLOTS;
                    if !(self.rt[k].armed() && self.rt[k].text() && self.rt_due(k)) {
                        continue;
                    }
                    if late_pin {
                        self.pin_part_step(&mut step, &list, p)?;
                        late_pin = false;
                    }
                    let cut = self.rt_text_cuts(k, p)?;
                    self.rt_count(k, cut);
                    if cut {
                        self.rt_first = k;
                        text_cut = true;
                        break;
                    }
                }
                if text_cut {
                    let end = self.hi.min(list.end_of(p));
                    self.granules_skipped += end - unit;
                    self.tally.skipped += end - unit;
                    self.tally.rt_skipped += end - unit;
                    self.g = end;
                    continue;
                }
            }
            // D3961: one walk iteration held a cancel 7-9 s (the loop polls at its top, D3887): each part-grain step is labelled
            // on its slow path, and the claim's first unit in a part polls after its verdict from the records (`entered`: no
            // per-granule load); a granule adds nothing
            match self.stage_part(&list, p, unit) {
                PartStep::Answered => return Ok(true),
                PartStep::Skipped => continue,
                PartStep::Granules => {}
            }
            if entered {
                step.tick().map_err(|_| err(WALK_CANCELED))?;
            }
            if late_pin {
                self.pin_part_step(&mut step, &list, p)?;
            }
            // P5-03 (the code verdict, `VERDICT_CODE_TABLE`): a part whose dictionary holds no code the leading code qual's
            // table may pass holds no survivor; the rest of the claim's granules in it are skipped before any lane opens
            if entered && self.order.is_none() && self.code_part_none(p)? {
                fires::fire_shared("pgrc.code_select");
                fires::fire_shared("pgrc.code_part");
                let end = self.hi.min(list.end_of(p));
                self.granules_skipped += end - unit;
                self.tally.skipped += end - unit;
                self.g = end;
                continue;
            }
            self.open_lanes(p)?;
            // D3902: a part's lanes opened and its decode are separate units (each stalled for seconds under reclaim)
            step.tick().map_err(|_| err(WALK_CANCELED))?;
            if self.recipe.entries.is_some() {
                self.open_dicts(p)?;
                step.tick().map_err(|_| err(WALK_CANCELED))?;
                match self.stage_entries(&list, p, unit, g)? {
                    PartStep::Answered => return Ok(true),
                    PartStep::Skipped => continue,
                    PartStep::Granules => {}
                }
            }
            // D3042, count.only: no lane is read; a part (or granule) is one window of its live rows
            if let Some(n) = self.count_only(list, p, unit, g) {
                if n == 0 {
                    continue;
                }
                return Ok(true);
            }
            // V4R-33: a count(*) over a part every granule of which the condition cache knows is one window of its total
            if entered && self.whole_part_hit(list, p, unit)? {
                if self.nrows == 0 {
                    continue;
                }
                return Ok(true);
            }
            self.granule_stats(p)?;
            if entered {
                self.late_meta_need(list, p, g)?;
                self.read_claim_run(&list, p, g);
                self.load_late_meta(p)?;
                step.tick().map_err(|_| err(WALK_CANCELED))?;
            }
            let lg = (g - list.starts()[p]) as u32;
            if self.unpin {
                self.unpin_window(p, lg, entered);
            }
            let rows = self.granule_rows(p, lg);
            self.verdict(p, lg, rows);
            if self.verdict == V_NONE {
                if self.recipe.skipped.is_some() {
                    self.count_skipped(visible_rows(list, p, lg, rows));
                }
                self.g = unit + 1;
                self.granules_skipped += 1;
                self.tally.skipped += 1;
                continue;
            }
            // a condition-cache hit decides the covered quals from its words, and the window is a covered one (the frame
            // skips its covered steps), so it leaves the filter-only lanes undecoded as an AllMatch window does
            let mut el = core::mem::take(&mut self.elided);
            self.lane_elision_into(self.verdict, self.cond_hit == V_ALL, &self.lane_all, &mut el);
            self.elided = el;
            if (self.verdict != V_ALL || self.on(VERDICT_ELIDE_HIT)) && self.elided.any() {
                self.tally.elided += self.elided.count() as u64;
            }
            // the answered form: every lane filter-only or answered, no delete mask (the records
            // count deleted rows), every answer exact — a window of `rows` and records, no decode; never under
            // VERDICT_ELIDE_HIT, whose filter lanes' quals the zone does not decide. SRC-21: a window `Mixed` on its
            // tallied predicates alone is answered too (the fold counts its rows from the tally)
            let deleted = list.parts()[p].deletes.as_ref().is_some_and(|d| d.granule_mask(lg).is_some());
            self.answered = (self.verdict == V_ALL || self.verdict == V_MIXED && self.tallied_win) && !self.on(VERDICT_ELIDE_HIT) && self.filter_lanes.union_covers(&self.answer_lanes, self.recipe.natts) && !deleted && self.answer(p, Some(lg));
            if self.answered {
                fires::fire_shared("pgrc.answered");
                self.nrows = rows as usize;
                self.has_dv = false;
                self.staged = g;
                self.g = unit + 1;
                self.granules_answered += 1;
                self.tally.answered += 1;
                return Ok(true);
            }
            // sortpath: a zone AllMatch window the zone covers passes every covered qual on every row (the frame elides the
            // covered steps and computes no words): the cache learns its all, so a later execution's hit takes it as one
            if self.verdict == V_ALL && self.cond_hit == V_MIXED && !deleted && self.on(gravity_program::protos::VERDICT_ZONE_COVERS) {
                if let Some((_, e)) = self.cond_entry.as_ref().filter(|(q, _)| *q == p) {
                    e.put(lg as usize, &[!0u64; VALIDITY_WORDS], rows as usize, 0);
                }
            }
            // decode.unread: the end counts a covered window whole (VERDICT_UNREAD), so a hit decodes no lane
            if self.cond_hit == V_ALL && self.on(VERDICT_UNREAD) {
                self.g = unit + 1;
                if self.unread(&list, p, lg, rows as usize)? == 0 {
                    self.granules_skipped += 1;
                    self.tally.skipped += 1;
                    continue;
                }
                self.staged = g;
                return Ok(true);
            }
            let staged = self.decode(&list, p, lg, g, rows as usize)?;
            // a miss publishes its words at the next advance: a Mixed granule with no delete mask. A row the host drops
            // fails a zone predicate (row_select's exact test; outside a PSMA window by PSMA's conservative bound), and
            // every zone predicate is one of the covered quals' (lowerv4 pgrc.rs declare_verdicts), so its clear bit is
            // their verdict whichever path ran. A staged window is always the host's selection (row_select), whose rows
            // are granule-relative like the bits the program sets through it (lowerv4 frame/site.rs row_index); a window
            // no row survives publishes NoMatch now. Under VERDICT_ELIDE_HIT the zone does not cover every covered op, so
            // the frame tests them all in a zone AllMatch window too (no row dropped there): its words publish as well
            debug_assert!(self.win.is_none() || !staged || self.selected, "a staged PSMA window is the host's selection (row_select)");
            // sortpath: a zone AllMatch window the zone does not cover (VERDICT_ZONE_COVERS off: no covered step elided) tests
            // every covered op too, VERDICT_ELIDE_HIT's case among them
            let tested = self.verdict == V_MIXED || (self.verdict == V_ALL && !self.on(gravity_program::protos::VERDICT_ZONE_COVERS));
            if self.cond_hit == V_MIXED && tested && !self.has_dv && (self.win.is_none() || !staged || self.selected) {
                if let Some((_, e)) = self.cond_entry.as_ref().filter(|(q, _)| *q == p) {
                    if staged {
                        self.cond_due = Some((Arc::clone(e), lg, rows as usize));
                    } else {
                        e.put(lg as usize, &[0; VALIDITY_WORDS], rows as usize, 0);
                    }
                }
            }
            self.g = unit + 1;
            if !staged {
                if self.recipe.skipped.is_some() {
                    self.count_skipped(visible_rows(list, p, lg, rows));
                }
                self.granules_skipped += 1;
                self.tally.skipped += 1;
                continue;
            }
            return Ok(true);
        }
    }

    /// decode.unread's window of granule `lg` of part `p` on a condition-cache hit: every lane elided (the code lanes'
    /// dictionaries open, as a window names them), no selection, its rows the hit's survivors (a Mixed hit's words, an All
    /// hit's every row) less the deleted rows; returns that count. The end counts it whole, reading no row.
    fn unread(&mut self, list: &PartList, p: usize, lg: u32, rows: usize) -> PgResult<usize> {
        self.open_dicts(p)?;
        let dv = list.parts()[p].deletes.as_ref().and_then(|d| d.granule_mask(lg));
        let nw = rows.div_ceil(64);
        // V4R-33: without a delete mask the hit's stored survivor count is the window's rows (the words' popcount)
        let stored = dv.is_none().then(|| self.cond_entry.as_ref().filter(|(q, _)| *q == p).and_then(|(_, e)| e.count(lg as usize))).flatten();
        let mut n = 0;
        // bounded("small-fixed: the mask words of one granule, at most VALIDITY_WORDS (128)")
        for w in (0..nw).filter(|_| stored.is_none()) {
            let mut m = if self.cond_mixed { self.cond_words[w] } else { !0 };
            if w + 1 == nw && rows % 64 != 0 {
                m &= (1u64 << (rows % 64)) - 1;
            }
            if let Some(d) = dv {
                m &= !d[w];
            }
            n += m.count_ones() as usize;
        }
        let n = stored.map_or(n, |c| c as usize);
        self.elided.clear();
        // bounded("key-columns: one bit per scanned column, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..self.recipe.natts {
            self.elided.set(k);
        }
        self.lane_byref.clear();
        self.has_dv = false;
        self.selected = false;
        self.nrows = n;
        self.tally.unread += 1;
        Ok(n)
    }

    /// V4R-33, decode.unread over a whole part: the claim entering part `p` at `unit` spans the part, the part has no
    /// delete vector, and the condition cache knows every granule of it (`Entry::part_total`): the part is ONE covered
    /// window of its survivor total (no lane decoded, the end counts it whole), or skipped when the total is 0 (nrows 0).
    /// - Visibility: a pgrc part is visible whole or not at all for the snapshot (the list is the snapshot's generation,
    ///   `resolve`); the only per-row visibility is the delete vector, so a part with one stays granule by granule.
    /// - Workers: a window never crosses a claim, and this fires only when this claim already holds the whole part (the
    ///   rule stage_part's answered part follows); the claim grain, not this window, decides how parts spread over workers,
    ///   so no worker-count threshold applies: one window replaces the claim's per-granule windows on the same worker.
    /// - Never under a walk order, the runtime filter, an entries recipe, or without the unread verdict.
    fn whole_part_hit(&mut self, list: &PartList, p: usize, unit: u64) -> PgResult<bool> {
        let spec = &list.parts()[p];
        if !self.on(VERDICT_UNREAD) || self.cond_fp == [0, 0] || self.order.is_some() || self.recipe.entries.is_some() || unit != list.starts()[p] || self.hi < list.end_of(p) || spec.deletes.is_some() || spec.rows > u32::MAX as u64 || self.rt_measuring() {
            return Ok(false);
        }
        if !self.cond_ready(p) {
            return Ok(false);
        }
        let Some(total) = self.cond_entry.as_ref().filter(|(q, _)| *q == p).and_then(|(_, e)| e.part_total()) else { return Ok(false) };
        self.open_dicts(p)?;
        let granules = spec.granules as u64;
        self.elided.clear();
        // bounded("key-columns: one bit per scanned column, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..self.recipe.natts {
            self.elided.set(k);
        }
        self.lane_byref.clear();
        self.lane_all.clear();
        self.has_dv = false;
        self.selected = false;
        self.answered = false;
        self.win = None;
        self.verdict = V_MIXED;
        self.cond_hit = V_ALL;
        self.cond_mixed = false;
        self.nrows = total as usize;
        self.staged = list.starts()[p];
        self.g = list.end_of(p);
        self.tally.cond[0] += granules;
        self.tally.unread += 1;
        COND_PARTS.fetch_add(1, Ordering::Relaxed);
        fires::fire_shared("pgrc.part_unread");
        if total == 0 {
            self.granules_skipped += granules;
            self.tally.skipped += granules;
        }
        Ok(true)
    }

    /// D3042, count.only (`VERDICT_COUNT_ONLY`): the end counts every window `end - cursor` and the site has no predicate,
    /// selection or dictionary lane, so the claim's next unit is one window of live rows with every lane elided and none
    /// prefetched or decoded: part `p` whole when the claim holds it from its first granule to its end (the manifest's rows
    /// less its delete vector's `deleted_rows`; a part is visible whole or not at all for the snapshot, `whole_part_hit`),
    /// else granule `g` (its rows less its delete mask's). `None` when the verdict is off or a walk order, an entries
    /// recipe, a sampler, a tid range or the runtime filter decides rows; `Some(0)` a unit with no live row (skipped).
    fn count_only(&mut self, list: &PartList, p: usize, unit: u64, g: u64) -> Option<usize> {
        if !self.on(VERDICT_COUNT_ONLY) || self.order.is_some() || self.recipe.entries.is_some() || self.recipe.sample.is_some() || self.nzone != 0 || !self.any.is_empty() || self.rt.iter().any(|s| s.lane != usize::MAX) || self.rt_measuring() {
            return None;
        }
        let spec = &list.parts()[p];
        let starts = list.starts()[p];
        let (n, staged, next, granules) = if unit == starts && self.hi >= list.end_of(p) && spec.rows <= u32::MAX as u64 {
            let deleted = spec.deletes.as_ref().map_or(0, |d| d.deleted_rows);
            (spec.rows.saturating_sub(deleted), starts, list.end_of(p), spec.granules as u64)
        } else {
            let lg = (g - starts) as u32;
            (visible_rows(list, p, lg, self.granule_rows(p, lg)), g, unit + 1, 1)
        };
        self.elided.clear();
        // bounded("key-columns: one bit per scanned column, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..self.recipe.natts {
            self.elided.set(k);
        }
        self.lane_byref.clear();
        self.lane_all.clear();
        self.has_dv = false;
        self.selected = false;
        self.answered = false;
        self.win = None;
        self.verdict = V_MIXED;
        self.nrows = n as usize;
        self.staged = staged;
        self.g = next;
        self.tally.unread += 1;
        fires::fire_shared("pgrc.count_only");
        if n == 0 {
            self.granules_skipped += granules;
            self.tally.skipped += granules;
        }
        Some(n as usize)
    }

    /// Decode granule `lg` of part `p` (global `g`) into the lane slots, the elided lanes left alone:
    /// the predicates' lanes and the code lanes whole, then the rest for the surviving rows only
    /// when few survive (`row_select`); false when none does (the granule is skipped).
    fn decode(&mut self, list: &PartList, p: usize, lg: u32, g: u64, rows: usize) -> PgResult<bool> {
        self.open_dicts(p)?;
        // D3156: a granule outside the site's kept table of a code lane holds no row the bind's zone predicates pass
        if self.kept_mode && self.kept_skips(p, lg) {
            fires::fire_shared("dict.kept_skip"); // shapecheck fire signal (D3156): a window outside a kept table's granules
            return Ok(false);
        }
        let natts = self.recipe.natts;
        self.lane_byref.clear();
        self.has_dv = match list.parts()[p].deletes.as_ref().and_then(|d| d.granule_mask(lg)) {
            Some(m) => {
                *self.dv = *m;
                true
            }
            None => false,
        };
        self.first_lanes();
        // decode.packed's host half (FRAME lever 1 a, VERDICT_PACKED_ZONE): a window no PSMA window narrows and no cache
        // hit decides, whose packed candidates all stage and hold every zone lane (the first op their only reader), has the
        // program test its filter on the raw deltas: the host neither decodes those zone lanes first nor selects
        let pre = if self.on(VERDICT_PACKED_ZONE) && self.win.is_none() && self.cond_hit != V_ALL && !self.has_dv && self.zones_packed() {
            self.packed_stage(p, lg, rows)?
        } else {
            0
        };
        if pre != 0 {
            // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
            for z in &self.zone[..self.nzone] {
                self.first.unset(z.lane as usize);
            }
        }
        // road code.sparse: its lane's non-base rows straight from the packed codes
        let sparse = if pre == 0 && self.on(VERDICT_CODE_SPARSE) { self.sparse_stage(p, lg, rows)? } else { None };
        // D3223 S1: a window `row_select` filters (the branch below that calls it, and its own entry test) compares the zone
        // lanes it can on their stored deltas (`enc_stage`): those decode after the selection, with the rest
        self.enc.clear();
        self.enc_set.clear();
        let selects = pre == 0 && sparse.is_none() && !self.cond_mixed && self.cond_hit != V_ALL && self.verdict == V_MIXED && (self.recipe.skipped.is_none() || self.on(VERDICT_COUNT_SELECT));
        if selects && (self.win.is_some() || (0..natts).any(|k| !self.elided.has(k) && !self.first.has(k))) {
            self.enc_stage(p, lg, rows);
        }
        // a tick per lane decoded whole (its granule read and decoded)
        let mut step = cancelpoll::step("granule_source decode: a window's first lanes");
        for k in 0..natts {
            if !self.elided.has(k) && self.first.has(k) && sparse.is_none_or(|(sk, _)| sk != k) {
                step.tick().map_err(|_| err(WALK_CANCELED))?;
                self.decode_lane(p, k, lg, rows, None)?;
            }
        }
        self.rest.clear();
        // bounded("key-columns: one lane bit per scanned column, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..natts {
            if !self.elided.has(k) && !self.first.has(k) {
                self.rest.set(k);
            }
        }
        // `sel_exact` is this window's `row_select`'s alone: a selection made another way (code.sparse's walk, which applies
        // its one table; the condition cache's) is no exact one over every zone predicate (D4952)
        self.sel_exact = false;
        let (selective, n) = if let Some((_, n)) = sparse {
            (true, n)
        } else if self.cond_mixed {
            match self.memo_select(p, lg, rows) {
                Some(n) => (true, n),
                None => self.cond_select(rows),
            }
        } else if self.cond_hit == V_ALL || pre != 0 {
            // a covering hit: every row passes the covered quals (its filter-only lanes may be elided: nothing to test);
            // a packed window: the program tests the zone lanes' deltas
            (false, 0)
        } else if self.rest.any() || self.win.is_some() {
            let (selective, n) = self.row_select(p, rows)?;
            // D1536 (VERDICT_COUNT_SELECT): a counting execution whose scan runs to its end counts the visible rows its
            // selection drops (each fails a zone predicate, so PostgreSQL's qual removes it); a window no row survives is
            // counted whole where `decode` returns
            if selective && n > 0 && self.recipe.skipped.is_some() {
                self.count_skipped(visible_rows(list, p, lg, rows as u32).saturating_sub(n as u64));
            }
            (selective, n)
        } else {
            (false, 0)
        };
        if selective && n == 0 {
            return Ok(false);
        }
        // decode.late: a late lane of a window that is no selection waits for `lane_fill` (its validity read now)
        self.late_pend = 0;
        self.late_g = (p, lg, rows);
        // a selection's window (VERDICT_GUARD_SEL: no late lane the program fills): the late guarded jsonb columns still
        // wait, its selected rows filled where a path lane is invalid (`guard_fill`); else every late lane decodes the selection now
        let late = match (selective, self.has_dv) {
            (_, true) => 0,
            (true, _) if self.on(VERDICT_GUARD_SEL) => self.guard_late,
            (true, _) => 0,
            (false, _) => self.late,
        };
        // D3223 S2: an exact selection (`row_select`'s `sel_exact`: every selected row satisfies every zone predicate) is a
        // window every row of which does, as an AllMatch one is: it publishes AllMatch, so a program whose zone covers its
        // leading filters runs its covered body over the selection (those steps not run again), and the filter-only lanes
        // an AllMatch window elides (`lane_elision_into`) are not decoded. Not in a window the condition cache records (its
        // words are the program's fill, which a covered body skips), a counting execution, or one whose late lanes wait
        if selective && self.sel_exact && late == 0 && self.recipe.skipped.is_none() && !(self.cond_hit == V_MIXED && self.cond_entry.as_ref().is_some_and(|(q, _)| *q == p)) {
            self.sel_all = true;
            let mut el = core::mem::take(&mut self.scratch_a);
            self.lane_elision_into(V_ALL, false, &self.lane_all, &mut el);
            // bounded("key-columns: one lane bit per scanned column, at most MaxTupleAttributeNumber (1664) columns")
            for k in 0..natts {
                if el.has(k) && self.rest.has(k) {
                    self.rest.unset(k);
                    self.elided.set(k);
                }
            }
            self.scratch_a = el;
            fires::fire_shared("pgrc.sel_exact"); // shapecheck fire signal (D3223 S2): a selection window published AllMatch
        }
        self.packed_win = if pre != 0 {
            pre
        } else if selective || self.has_dv || self.packed == 0 {
            0
        } else {
            self.packed_stage(p, lg, rows)?
        };
        if self.packed != 0 {
            self.tally.packed[(self.packed_win == 0) as usize] += 1;
        }
        // each rest lane gathers the selection's rows or decodes whole, its rows at their positions as a gather leaves them,
        // whichever its stream's fitted costs make cheaper (decode_lane, `gather_pays`; H09)
        for k in 0..natts {
            if !self.rest.has(k) {
                continue;
            }
            // an encoded zone lane decodes as the first decode would have (never late, never packed)
            if k < 64 && (late | self.packed_win) >> k & 1 != 0 && !self.enc_set.has(k) {
                self.late_stage(p, k, lg, rows)?;
            } else {
                self.decode_lane(p, k, lg, rows, selective.then_some(n))?;
            }
        }
        if late != 0 && !self.recipe.late_guards.is_empty() {
            self.guard_fill(late, rows, selective.then_some(n))?;
        }
        self.selected = selective || self.has_dv;
        if self.has_dv && !selective {
            self.sel.clear();
            self.sel.extend((0..rows as u32).filter(|&r| !PartDeletes::is_deleted(&self.dv, r)));
        }
        self.nrows = rows;
        self.staged = g;
        self.granules_decoded += 1;
        self.tally.decoded += 1;
        if selective {
            self.granules_selected += 1;
            self.tally.selected += 1;
        }
        Ok(true)
    }

    /// M6 D2: each late guarded jsonb column lane (`late_guards`) filled where one of its path lanes is invalid, the `{}`
    /// image in every other row's cell (the program reads the cell only on a valid path row's untaken branch; a later
    /// fill of those rows replaces it: they are not done). A selection's window (`sel`: its `n` rows) fills its rows
    /// alone, the only ones the program reads. jsonops: an invalid row of a path lane this part elected, where that is
    /// SQL NULL (`guard_null`), is not filled: its compare over `{}` is PostgreSQL's over the document. A SQL NULL
    /// document is NULL by its validity, read at `late_stage`.
    fn guard_fill(&mut self, late: u64, rows: usize, sel: Option<usize>) -> PgResult<()> {
        let guards = Arc::clone(&self.recipe.late_guards);
        let part = self.late_g.0;
        let mut keep = [u64::MAX; VALIDITY_WORDS];
        if let Some(n) = sel {
            keep = [0; VALIDITY_WORDS];
            // bounded("small-fixed: the selected rows of one granule, at most GRANULE_ROWS (8192)")
            for &r in self.sel[..n].iter().filter(|&&r| (r as usize) < rows) {
                keep[r as usize / 64] |= 1 << (r % 64);
            }
        }
        let mut seen = 0u64;
        // a tick per guarded column: its selected rows' documents are decoded (variable-size copies)
        let mut step = cancelpoll::step("granule_source guard_fill: a guarded column's documents");
        for &(col, _) in guards.iter() {
            step.tick().map_err(|_| err(WALK_CANCELED))?;
            let c = col as usize;
            if c >= 64 || late >> c & 1 == 0 || self.late_pend >> c & 1 == 0 || seen >> c & 1 != 0 {
                continue;
            }
            seen |= 1 << c;
            self.datums[c * ROWS..c * ROWS + rows].fill(EMPTY_OBJECT.0.as_ptr() as u64);
            // the guard lanes read as NULL where invalid: elected by this part and `guard_null`
            let null = guards.iter().filter(|g| g.0 == col && g.1 < 64 && self.guard_null >> g.1 & 1 != 0 && !self.absent(part, g.1 as usize)).fold(0u64, |m, g| m | 1 << g.1);
            let mut sel = core::mem::take(&mut self.guard_sel);
            sel.clear();
            // bounded("small-fixed: the mask words of one granule, at most VALIDITY_WORDS (128)")
            for w in 0..rows.div_ceil(64) {
                let mut inv = 0u64;
                // sortpath: an elided path lane (a covered window: no compare reads the cell) guards no row
                // bounded("key-columns: the path lanes that guard one jsonb column, at most MaxTupleAttributeNumber (1664) columns")
                for &(_, p) in guards.iter().filter(|g| g.0 == col && !self.elided.has(g.1 as usize)) {
                    let p = p as usize;
                    if !self.all_valid[p] && !(p < 64 && null >> p & 1 != 0) {
                        inv |= !self.valid[p * VALIDITY_WORDS + w];
                    }
                }
                inv &= keep[w];
                if rows - w * 64 < 64 {
                    inv &= (1u64 << (rows - w * 64)) - 1;
                }
                // bounded("small-fixed: the set bits of one 64-bit word")
                while inv != 0 {
                    sel.push((w * 64 + inv.trailing_zeros() as usize) as u32); // alloc: Node (kept at its high-water mark)
                    inv &= inv - 1;
                }
            }
            let r = if sel.is_empty() { Ok(1) } else { self.late_fill(c, &sel) };
            self.guard_sel = sel;
            r?;
        }
        Ok(())
    }

    /// Every zone predicate's lane is a packed candidate (bit l of `packed`: site lane l, below 64).
    fn zones_packed(&self) -> bool {
        self.packed != 0 && self.nzone > 0 && self.zone[..self.nzone].iter().all(|z| z.lane < 64 && self.packed >> z.lane & 1 != 0)
    }

    /// The lanes decoded whole before any selection (`first`): the code lanes and the predicates' lanes.
    fn first_lanes(&mut self) {
        self.first.clear();
        // bounded("key-columns: one bit per scanned column, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..self.recipe.natts {
            if self.recipe.kinds[k] == lane::KIND_CODES {
                self.first.set(k);
            }
        }
        // the predicates' lanes serve row_select, which a condition-cache hit never runs: theirs decode with the rest
        if self.verdict == V_MIXED && self.cond_hit != V_ALL {
            // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
            for z in &self.zone[..self.nzone] {
                self.first.set(z.lane as usize);
            }
        }
    }

    /// decode.packed: every candidate's frames into its `PackedFrames`, or none (0, the window decodes as before): each is a
    /// by-value integer lane the window does not elide, stored BYTE_FOR at delta width 1 or 2 or Const, every frame's
    /// reference inside the lane's words (`packed_words`) and `reference + 2^(8 width) - 1` inside i64 (so `reference +
    /// delta` is exact).
    fn packed_stage(&mut self, p: usize, lg: u32, rows: usize) -> PgResult<u64> {
        use pgrc_codec::section::{frame_base, frame_start, open_section, payload_region};
        let natts = self.recipe.natts;
        // a tick per candidate lane: each reads its granule's frames
        let mut step = cancelpoll::step("granule_source decode.packed: a window's packed frames");
        let mut m = self.packed;
        while m != 0 {
            step.tick().map_err(|_| err(WALK_CANCELED))?;
            let k = m.trailing_zeros() as usize;
            m &= m - 1;
            let lw = self.recipe.widths[k] as u32;
            if self.elided.has(k) || self.recipe.kinds[k] != lane::KIND_BYVAL || self.recipe.numeric.has(k) || !matches!(lw, 2 | 4 | 8) {
                return Ok(0);
            }
            let unsigned = self.recipe.profiles.get(k).and_then(|p| p.as_ref()).is_some_and(|p| matches!(p.class, StorageClass::ByvalWord { signed: false, .. }));
            let (lo, hi) = packed_words(lw, unsigned);
            // lane k's table words past the lane slots
            let t = raw::packed_frames(&mut self.datums[natts * ROWS + k * PF_WORDS..]);
            t.n = rows.div_ceil(FRAME_VALUES as usize) as u64;
            let cur = self.cursors[p * natts + k].as_deref_mut().expect("cursor");
            let (enc, w) = (cur.key().encoding, cur.key().width as usize);
            let fits = if enc == EncodingId::Const as u16 {
                let (mut one, mut small) = ([0u64; 1], [0u64; 8]);
                let arena = ByteArena::new(super::raw::words_as_bytes_mut(&mut small));
                cur.decode_sel(lg, &Selection { rows: &[0] }, &mut DecodeOut { datums: &mut one, arena }).map_err(read_err)?;
                let r = one[0] as i64;
                // bounded("small-fixed: the frames of one granule, at most FRAMES_PER_GRANULE (8)")
                for (f, fr) in t.frames[..t.n as usize].iter_mut().enumerate() {
                    (fr.deltas, fr.width, fr.reference, fr.fmax, fr.first_row) = (0, 0, r, r, (f * FRAME_VALUES as usize) as u64);
                }
                (lo..=hi).contains(&r)
            } else if enc == EncodingId::ByteFor as u16 && matches!(w, 1 | 2) {
                cur.granule_ctx(lg, |ctx| {
                    let hdr = open_section(ctx.bytes, enc)?;
                    let (payload, fbase) = (payload_region(&hdr, ctx.bytes)?, frame_base(&hdr, ctx.bytes, ctx.granule_in_extent)?);
                    if ctx.values as usize != rows {
                        return Ok(false);
                    }
                    // bounded("small-fixed: the frames of one granule, at most FRAMES_PER_GRANULE (8)")
                    for (f, fr) in t.frames[..t.n as usize].iter_mut().enumerate() {
                        let (at, n) = (frame_start(ctx.frame_table, fbase + f as u32)?, (rows - f * FRAME_VALUES as usize).min(FRAME_VALUES as usize));
                        let body = payload.get(at..at + 8 + n * w).ok_or(pgrc_format::FormatError::Bounds { at: "byte-for frame payload" })?;
                        let r = i64::from_le_bytes(body[..8].try_into().expect("8 bytes"));
                        let Some(bound) = r.checked_add((1 << (8 * w)) - 1).filter(|_| (lo..=hi).contains(&r)) else { return Ok(false) };
                        (fr.deltas, fr.width, fr.reference, fr.fmax, fr.first_row) = (body[8..].as_ptr() as u64, w as u64, r, bound.min(hi), (f * FRAME_VALUES as usize) as u64);
                    }
                    Ok(true)
                })
                .map_err(read_err)?
            } else {
                false
            };
            if !fits {
                return Ok(0);
            }
        }
        Ok(self.packed)
    }

    /// In-place read: lane `k`'s whole granule `lg` read in place instead of copied
    /// into its slot — a by-value lane of width 2 / 4 / 8 whose values stream is a root VERBATIM stream stored at the
    /// lane's width (the bytes `decode_full` / `decode_i32` / `decode_i16` copy unchanged), the body aligned to the width,
    /// with `IN_PLACE_SLACK` bytes of the extent image after it. Never a selection (a gather writes rows at their
    /// positions), a numeric lane (mantissas / images) or a zone lane (the host's filter reads its slot). A late lane never
    /// comes here (`late_decode` fills the slot a published lane already points at). The pointer is published for the
    /// staged window only (`stage` clears it): the cursor keeps the extent image until it loads another values extent,
    /// which happens at a later `stage`. Consumers only read a lane.
    fn in_place(&mut self, p: usize, k: usize, kind: u8, lg: u32, rows: usize, sel: Option<usize>) -> PgResult<Option<*const u8>> {
        let w = self.recipe.widths[k];
        if kind != lane::KIND_BYVAL || sel.is_some() || !matches!(w, 2 | 4 | 8) || self.recipe.numeric.has(k) {
            return Ok(None);
        }
        let got = if self.zone[..self.nzone].iter().any(|z| z.lane as usize == k) {
            None
        } else {
            let cur = self.cursors[p * self.recipe.natts + k].as_deref_mut().expect("cursor");
            if cur.key().width != w {
                None
            } else {
                cur.verbatim_body(lg, IN_PLACE_SLACK).map_err(read_err)?.filter(|b| b.len() == rows * w as usize && b.as_ptr() as usize % w as usize == 0).map(|b| b.as_ptr())
            }
        };
        self.tally.in_place[got.is_some() as usize] += 1;
        Ok(got)
    }

    /// Lane `k`'s `ncodes_or_scale` in part `p`: a numeric(p, s) lane's typmod scale, a shred number path's scale; N14 a
    /// `dyn_scale` lane's the scale its stream in part `p` elected (the stream entry's `aux32`: every granule of a
    /// PACKED_NUMERIC stream carries it, `decode_mantissa` checks each against it; a verbatim stream's is not read).
    pub(super) fn lane_scale(&self, p: usize, k: usize) -> u32 {
        if !self.recipe.dyn_scale.has(k) {
            return self.recipe.scales[k];
        }
        let cur = self.cursors.get(p.wrapping_mul(self.recipe.natts).wrapping_add(k)).and_then(|c| c.as_deref());
        cur.filter(|c| c.key().encoding == EncodingId::PackedNumeric as u16).map_or(0, |c| c.entry().aux32)
    }

    /// jsonops (3): presence lane `k` of granule `lg` (`GranuleRecipe::with_shred_presence_lane`): the path lane's validity
    /// as words, valid where the document (its column's lane in this recipe) is not NULL, in a part whose path lane is
    /// sealed STREAMF_SHRED_OBJECT_ROOTS; all-invalid elsewhere (no flag, no document lane, or the path not elected, which
    /// `decode_lane` publishes before reaching here).
    fn decode_presence(&mut self, p: usize, k: usize, lg: u32, rows: usize) -> PgResult<()> {
        let natts = self.recipe.natts;
        let attno = self.recipe.attnos[k];
        let doc = (0..natts).find(|&c| self.recipe.attnos[c] == attno && self.recipe.shred_of(c).is_none() && self.cursors[p * natts + c].is_some());
        let flagged = self.cursors[p * natts + k].as_deref().is_some_and(|c| c.entry().flags & pgrc_format::part::STREAMF_SHRED_OBJECT_ROOTS != 0);
        let mut bits = [0u64; VALIDITY_WORDS];
        let mut docv = [0u64; VALIDITY_WORDS];
        let ones = |b: &mut [u64; VALIDITY_WORDS]| {
            b.fill(0);
            // bounded("small-fixed: the rows of one granule, at most GRANULE_ROWS (8192); the claim's walk ticks per granule")
            for r in 0..rows {
                b[r / 64] |= 1 << (r % 64);
            }
        };
        let known = match (flagged, doc) {
            (true, Some(c)) => {
                let cur = self.cursors[p * natts + k].as_deref_mut().expect("cursor");
                if cur.rows_in_granule(lg) as usize != rows {
                    return Err(err("gravity: pgrc granule row skew"));
                }
                if !cur.has_validity_stream() || matches!(cur.validity(lg, &mut bits).map_err(read_err)?, ValidityVerdict::AllValid) {
                    ones(&mut bits);
                }
                let dcur = self.cursors[p * natts + c].as_deref_mut().expect("cursor");
                if !dcur.has_validity_stream() || matches!(dcur.validity(lg, &mut docv).map_err(read_err)?, ValidityVerdict::AllValid) {
                    ones(&mut docv);
                }
                true
            }
            _ => false,
        };
        // shapecheck fire signals (jsonops (3)): a granule's presence lane answered, or every row takes PG's test
        fires::fire_shared(if known { "jsonb.presence" } else if flagged { "jsonb.presence_nodoc" } else { "jsonb.presence_unflagged" });
        // bounded("small-fixed: the rows of one granule, at most GRANULE_ROWS (8192); the claim's walk ticks per granule")
        for r in 0..rows {
            self.datums[k * ROWS + r] = u64::from(known && bits[r / 64] >> (r % 64) & 1 != 0);
        }
        self.valid[k * VALIDITY_WORDS..(k + 1) * VALIDITY_WORDS].copy_from_slice(if known { &docv } else { &[0u64; VALIDITY_WORDS] });
        self.all_valid[k] = false;
        self.inplace[k] = core::ptr::null();
        Ok(())
    }

    /// D3038 (R3): character lane `k`'s lengths of granule `lg` into `char_lens`, from the octet lengths `octet_lens` holds
    /// for it (valid rows `all` / `bits`): the same where the column's granule is all ASCII (its Stats witness,
    /// `STATSF_ALL_ASCII`); a coded part's per code from its dictionary index's char field (or its payload); else each
    /// value's UTF-8 count over the column's own stream (a second cursor, `char_src`, where the lane reads the stored
    /// octet lane).
    #[allow(clippy::too_many_arguments)]
    fn char_lengths(&mut self, p: usize, k: usize, lg: u32, rows: usize, coded: bool, stored: bool, all: bool, bits: &[u64; VALIDITY_WORDS]) -> PgResult<()> {
        let natts = self.recipe.natts;
        if self.char_lens.len() < ROWS {
            self.char_lens.resize(ROWS, 0); // alloc: Node
        }
        let attno = self.recipe.attnos[k] as u32;
        let part = Arc::clone(self.parts().part(p, &self.recipe.registry)?);
        let live = |r: usize| all || bits[r / 64] >> (r % 64) & 1 != 0;
        if !coded {
            let ascii = self.parts().body(p, attno, &part)?.and_then(|b| b.get(lg as usize * STATS_RECORD_LEN..(lg as usize + 1) * STATS_RECORD_LEN)).is_some_and(|r| StatsRecord::decode(&mut pgrc_format::wire::Cur::new(r)).is_ok_and(|r| r.flags & pgrc_format::meta::STATSF_ALL_ASCII != 0));
            if ascii {
                self.char_lens[..rows].copy_from_slice(&self.octet_lens[..rows]);
                fires::fire_shared("octet.chars_ascii");
                return Ok(());
            }
            let i = p * natts + k;
            if stored {
                if self.char_src.len() <= i {
                    self.char_src.resize_with(i + 1, || None); // alloc: Node
                }
                self.note_part(p);
                if self.char_src[i].is_none() {
                    // FASTADD: the column's own stream in the lane's part (a stand-in for a column the part lacks)
                    let lane = Arc::clone(self.parts().lane(p, self.face(k), &part)?);
                    let mut cur = Box::new(StreamCursor::open(lane, pgrc_am::scan::codec_binding(), attno, 0).map_err(read_err)?); // alloc: Exec
                    cur.set_pass_behind(self.unpin);
                    self.pool_cursor(&mut cur);
                    cur.set_owner(self.held.owner());
                    self.char_src[i] = Some(cur);
                    self.cursors_held += CURSOR_BYTES;
                }
            }
            let cur = if stored { self.char_src[i].as_deref_mut() } else { self.cursors[i].as_deref_mut() }.expect("cursor");
            self.scratch.granule_chars(cur, lg, &mut self.char_lens[..rows]).map_err(read_err)?;
            fires::fire_shared("octet.chars_decoded");
            return Ok(());
        }
        self.open_dict(p, k)?;
        let face = self.face(k);
        let rp = self.rel_unbound();
        let Some(Dict::Part(d)) = rp.dict_if_open(p, face).map(|e| &e.dict) else { return Err(err("gravity: a character lane's coded part without its dictionary")) };
        let cur = self.cursors[p * natts + k].as_deref_mut().expect("cursor");
        let codes = &mut self.char_lens[..rows];
        if cur.decode_codes(lg, codes).map_err(read_err)? as usize != rows {
            return Err(err("gravity: pgrc character lane decoded count"));
        }
        use pgrc_format::dict::DictCharLenForm;
        const AT: usize = pgrc_format::dict::DICT_INDEX_ENTRY_LEN;
        let (index, form) = (d.index_bytes().map_err(read_err)?, d.charlen_form());
        let payload = if matches!(form, DictCharLenForm::Absent) { d.payload_bytes().map_err(read_err)? } else { None };
        let word = |b: &[u8], at: usize| b.get(at..at + 4).map(|w| u32::from_le_bytes(w.try_into().expect("len 4")));
        // a tick per row: a row with no stored length counts its value's characters
        let mut step = cancelpoll::step("granule_source char_lengths: a granule's character counts");
        for (r, c) in codes.iter_mut().enumerate() {
            step.tick().map_err(|_| err(WALK_CANCELED))?;
            if !live(r) {
                *c = 0;
                continue;
            }
            let at = *c as usize * AT;
            let (Some(off), Some(len)) = (word(&index, at), word(&index, at + 4)) else { return Err(err("gravity: pgrc character lane code past its dictionary")) };
            *c = match form {
                DictCharLenForm::Absolute => word(&index, at + 8),
                DictCharLenForm::Delta => word(&index, at + 8).and_then(|f| len.checked_sub(f)),
                DictCharLenForm::Absent => payload.and_then(|b| b.get(off as usize + 4..off as usize + 4 + len as usize)).map(|v| crate::dict::text_hosts::utf8_chars(v) as u32),
            }
            .ok_or_else(|| err("gravity: pgrc character lane length past its dictionary"))?;
        }
        fires::fire_shared("octet.chars_coded");
        Ok(())
    }

    /// D3038: octet lane `k` of granule `lg` (`GranuleRecipe::with_octet_lane`): each row's payload byte length of the
    /// column, valid where the column is not NULL. A part that sealed the column's octet-length lane reads it (R1: its
    /// cursor is that lane's, `Faces::locate`); a coded part's lengths are its dictionary index's per code (its codes
    /// decoded, never its bytes); any other part's from `TextScratch::granule_lengths` (a verbatim stream's entry
    /// headers, another encoding's decoded images).
    fn decode_octet(&mut self, p: usize, k: usize, lg: u32, rows: usize) -> PgResult<()> {
        let natts = self.recipe.natts;
        let face = self.face(k);
        let key = self.cursors[p * natts + k].as_deref().expect("cursor").key();
        let stored = key.class == pgrc_format::class::CLASS_BYVAL;
        let coded = !stored && key.encoding == EncodingId::DictCodes as u16;
        let col = self.recipe.attnos[k];
        let memo = self.octet_memo.filter(|&(mp, mc, mg, _)| (mp, mc, mg) == (p, col, lg) && self.octet_lens.len() >= rows);
        if coded && memo.is_none() {
            self.open_dict(p, k)?;
        }
        let rp = self.rel_unbound();
        if self.octet_lens.len() < ROWS {
            self.octet_lens.resize(ROWS, 0); // alloc: Node
        }
        let cur = self.cursors[p * natts + k].as_deref_mut().expect("cursor");
        if cur.rows_in_granule(lg) as usize != rows {
            return Err(err("gravity: pgrc granule row skew"));
        }
        let lens = &mut self.octet_lens[..rows];
        let mut bits = [0u64; VALIDITY_WORDS];
        let all = if let Some((_, _, _, all)) = memo {
            bits = self.octet_bits;
            fires::fire_shared("octet.lane_shared");
            all
        } else if stored {
            let words = &mut self.octet_words;
            words.resize(rows, 0); // alloc: Node
            if cur.decode_i32(lg, words).map_err(read_err)? as usize != rows {
                return Err(err("gravity: pgrc octet lane decoded count"));
            }
            let all = !cur.has_validity_stream() || matches!(cur.validity(lg, &mut bits).map_err(read_err)?, ValidityVerdict::AllValid);
            // bounded("small-fixed: the rows of one granule, at most GRANULE_ROWS (8192); the claim's walk ticks per granule")
            for (r, (l, &w)) in lens.iter_mut().zip(words.iter()).enumerate() {
                *l = if all || bits[r / 64] >> (r % 64) & 1 != 0 { w as u32 } else { 0 };
            }
            fires::fire_shared("octet.stored");
            all
        } else if coded {
            let Some(Dict::Part(d)) = rp.dict_if_open(p, face).map(|e| &e.dict) else { return Err(err("gravity: an octet lane's coded part without its dictionary")) };
            if cur.decode_codes(lg, lens).map_err(read_err)? as usize != rows {
                return Err(err("gravity: pgrc octet lane decoded count"));
            }
            let all = !cur.has_validity_stream() || matches!(cur.validity(lg, &mut bits).map_err(read_err)?, ValidityVerdict::AllValid);
            let index = d.index_bytes().map_err(read_err)?;
            const AT: usize = pgrc_format::dict::DICT_INDEX_ENTRY_LEN;
            // bounded("small-fixed: the rows of one granule, at most GRANULE_ROWS (8192); the claim's walk ticks per granule")
            for (r, l) in lens.iter_mut().enumerate() {
                if !all && bits[r / 64] >> (r % 64) & 1 == 0 {
                    *l = 0;
                    continue;
                }
                let at = *l as usize * AT;
                let rec = index.get(at + 4..at + 8).ok_or_else(|| err("gravity: pgrc octet lane code past its dictionary"))?;
                *l = u32::from_le_bytes(rec.try_into().expect("len 4"));
            }
            all
        } else {
            let all = self.scratch.granule_lengths(cur, lg, lens).map_err(read_err)?;
            if !all {
                let w = rows.div_ceil(64);
                bits[..w].copy_from_slice(&self.scratch.valid_words()[..w]);
            }
            all
        };
        self.octet_memo = Some((p, col, lg, all));
        self.octet_bits = bits;
        let chars = self.recipe.shred_of(k).is_some_and(|s| s.chars);
        if chars {
            self.char_lengths(p, k, lg, rows, coded, stored, all, &bits)?;
        }
        let lens = if chars { &self.char_lens[..rows] } else { &self.octet_lens[..rows] };
        let out = &mut self.datums[k * ROWS..k * ROWS + rows];
        if self.recipe.shred_of(k).is_some_and(|s| s.nonempty) {
            // R2: the emptiness lane's word is the length's test
            // bounded("small-fixed: the rows of one granule, at most GRANULE_ROWS (8192); the claim's walk ticks per granule")
            for (o, &l) in out.iter_mut().zip(lens.iter()) {
                *o = (l != 0) as u64;
            }
        } else {
            // bounded("small-fixed: the rows of one granule, at most GRANULE_ROWS (8192); the claim's walk ticks per granule")
            for (o, &l) in out.iter_mut().zip(lens.iter()) {
                *o = l as u64;
            }
        }
        if all {
            self.valid[k * VALIDITY_WORDS..(k + 1) * VALIDITY_WORDS].fill(!0);
        } else {
            self.valid[k * VALIDITY_WORDS..(k + 1) * VALIDITY_WORDS].copy_from_slice(&bits);
        }
        self.all_valid[k] = all;
        self.inplace[k] = core::ptr::null();
        fires::fire_shared(if coded { "octet.lane_coded" } else { "octet.lane_rows" });
        Ok(())
    }

    /// V4R-187: `sel16` as `sel`'s rows, once per selection (a granule holds at most ROWS rows, below 2^16).
    fn ensure_sel16(&mut self) {
        if !self.sel16_ok {
            self.sel16.clear();
            self.sel16.extend(self.sel.iter().map(|&r| r as u16));
            self.sel16_ok = true;
        }
    }

    /// Lane `k` of granule `lg`: whole, or the `n` rows of `sel16` gathered by `decode_sel` and scattered to their positions
    /// where the gather costs less than the whole decode (`gather_pays`).
    fn decode_lane(&mut self, p: usize, k: usize, lg: u32, rows: usize, sel: Option<usize>) -> PgResult<()> {
        let natts = self.recipe.natts;
        if self.cursors[p * natts + k].is_none() && self.absent(p, k) {
            // a shred lane this part did not elect (at its kind and scale): every row invalid, so every row walks its
            // image (M6 D2); the cells zeroed, never read
            self.datums[k * ROWS..k * ROWS + rows].fill(0);
            self.valid[k * VALIDITY_WORDS..(k + 1) * VALIDITY_WORDS].fill(0);
            self.all_valid[k] = false;
            self.inplace[k] = core::ptr::null();
            return Ok(());
        }
        if self.recipe.shred_of(k).is_some_and(|s| s.presence) {
            return self.decode_presence(p, k, lg, rows);
        }
        if self.recipe.shred_of(k).is_some_and(|s| s.octet) {
            return self.decode_octet(p, k, lg, rows);
        }
        // a code lane's dictionary, taken before the cursor's borrow (D3156: a kept-mode site's own, kept.rs `dict_at`)
        let entry = if self.recipe.kinds[k] == lane::KIND_CODES { self.dict_at(p, k) } else { None };
        let cur = self.cursors[p * natts + k].as_deref_mut().expect("cursor");
        if cur.rows_in_granule(lg) as usize != rows {
            return Err(err("gravity: pgrc granule row skew"));
        }
        let mut small = [0u64; 8];
        let numeric = self.recipe.numeric.has(k);
        let packed = numeric && cur.key().encoding == EncodingId::PackedNumeric as u16;
        // a numeric lane: the mantissas from a packed stream's frames; a verbatim part's images by reference for this granule
        let kind = if numeric && !packed {
            self.lane_byref.set(k);
            lane::KIND_BYREF
        } else {
            self.recipe.kinds[k]
        };
        // H09: the selection's rows gathered only where the stream's fitted costs make the gather the cheaper decode
        let sel = sel.filter(|&n| gather_pays(cur.key(), kind, self.recipe.widths[k], rows, &self.sel[..n]));
        // a by-reference lane decodes its images into its own arena (grown to the high-water mark on
        // `ArenaExhausted`, never per granule); the others never touch the 64-byte one
        let (want, decoded) = if packed {
            if self.recipe.dyn_scale.has(k) {
                fires::fire_shared("pgrc.packed_dyn"); // shapecheck fire signal (N14): a min / max lane reads a chunk's packed codes
            }
            let scale = self.lane_scale(p, k) as i32;
            if sel.is_some() {
                self.ensure_sel16();
            }
            let cur = self.cursors[p * natts + k].as_deref_mut().expect("cursor");
            raw::decode_lane_mantissa(cur, scale, lg, rows, sel, &mut self.datums[k * ROWS..], &self.sel16)
        } else if kind == lane::KIND_CODES {
            let codes = super::raw::words_as_u32_mut(&mut self.datums[k * ROWS..(k + 1) * ROWS], rows);
            let r = match &entry.expect("dictionary").dict {
                Dict::Part(_) => cur.decode_codes(lg, codes),
                Dict::Ranked { table, .. } => {
                    let ranks = table.ranks().expect("ranks");
                    // D3156: a kept table covers every granule a source decodes (`kept_skips` at decode)
                    if !ranks.covers(lg) {
                        return Err(err("gravity: Reason::Bug: D3156: a verbatim part's kept-granule codes decoded outside their granules"));
                    }
                    let ranked = ranks.granule_codes(lg);
                    codes[..ranked.len()].copy_from_slice(ranked);
                    Ok(ranked.len() as u32)
                }
            };
            (rows, r)
        } else if let Some(b) = self.in_place(p, k, kind, lg, rows, sel)? {
            self.inplace[k] = b;
            (rows, Ok(rows as u32))
        } else {
            // a gathered decode reads `sel16` (framed_at, decode_sel); the words' walk left it stale (V4R-187)
            if sel.is_some() {
                self.ensure_sel16();
            }
            // bounded("small-fixed: one retry per doubling of the lane's arena chunk, at most 64 (a usize's bits)")
            loop {
            let arena: &mut [u8] = if kind == lane::KIND_BYREF {
                if self.arenas[k].is_empty() {
                    self.arenas[k].resize(ARENA_FIRST, 0); // alloc: Cold
                }
                &mut self.arenas[k]
            } else {
                super::raw::words_as_bytes_mut(&mut small)
            };
            let cur = self.cursors[p * natts + k].as_deref_mut().expect("cursor");
            let (want, r) = raw::decode_lane_into(cur, kind, self.recipe.widths[k], lg, rows, sel, &mut self.datums[k * ROWS..], &mut self.dense, &self.sel16, &self.sel, arena, &mut 0);
            match r {
                Err(pgrc_read::ReadError::Format(pgrc_format::FormatError::ArenaExhausted { .. })) if kind == lane::KIND_BYREF && double_arena(&mut self.arenas[k]) => {}
                r => break (want, r),
            }
            }
        };
        if decoded.map_err(read_err)? as usize != want {
            return Err(err("gravity: pgrc granule decoded count"));
        }
        let cur = self.cursors[p * natts + k].as_deref_mut().expect("cursor");
        self.all_valid[k] = !cur.has_validity_stream() || matches!(cur.validity(lg, &mut self.valid[k * VALIDITY_WORDS..(k + 1) * VALIDITY_WORDS]).map_err(read_err)?, ValidityVerdict::AllValid);
        Ok(())
    }
}

impl GranuleRangeSource {
    /// One granule as a window: the decoded lanes as they lie, the live rows as a selection under a delete mask.
    fn window_pg(&mut self, w: &mut Window<'_>) -> PgResult<u32> {
        // the previous window was consumed whole (the program asked for the next): its miss words publish now
        if let Some((e, lg, rows)) = self.cond_due.take() {
            // a sparse granule keeps its survivors beside its words: a later hit's selection (memo_select) at the gather's
            // crossover, the words path above it
            e.put(lg as usize, &self.cond_words[..], rows, rows / SELECT_DIV);
        }
        // a code lane's window names the part's dictionary even when the window was answered from
        // its records: the lanes open here when nothing decoded them
        if self.cur_part != usize::MAX && self.answered && self.recipe.kinds[..self.recipe.natts].contains(&lane::KIND_CODES) {
            let _step = cancelpoll::step("granule_source window: an answered part's lanes and dictionaries opened");
            self.open_lanes(self.cur_part)?;
            self.open_dicts(self.cur_part)?;
        }
        self.late_pend = 0;
        self.packed_win = 0;
        if !self.stage()? {
            return Ok(0);
        }
        let rows = self.nrows;
        let ck = if self.on(VERDICT_CONST_KEY) && !self.answered { self.const_key_window()? } else { None };
        // M6 R3: each code lane's dictionary is named below; a shred path's the part did not elect is located (none opens)
        let mut step = cancelpoll::step("granule_source window: a code lane's dictionary opened");
        for k in 0..self.recipe.natts {
            if !self.answered && self.recipe.kinds[k] == lane::KIND_CODES && self.dict_at(self.cur_part, k).is_none() {
                step.tick().map_err(|_| err(WALK_CANCELED))?;
                self.open_dict(self.cur_part, k)?;
            }
        }
        let list = self.list.as_ref().expect("bound");
        if self.answered {
            w.answer(self.answers.as_ptr());
            w.rows = rows as u32;
            w.set_base(WindowBase { pos: ScanPos::new(self.recipe.rel, self.staged as u32, 0), part_id: self.cur_part as u32, dict_epoch: 0, granule: (self.staged - list.starts()[self.cur_part]) as u32, claim_id: self.lo as u32 });
            return Ok(rows as u32);
        }
        let mut epoch = 0u32;
        // bounded("key-columns: one lane per scanned column, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..self.recipe.natts {
            let id = lane::id(lane::ID_ATTR, self.recipe.attnos[k] as u32);
            let mut l = if self.elided.has(k) {
                Lane::byval(core::ptr::null(), core::ptr::null(), self.recipe.widths[k], lane::REGION_ARENA, id)
            } else {
                let valid = if self.all_valid[k] { core::ptr::null() } else { self.valid[k * VALIDITY_WORDS..].as_ptr() };
                let ptr = if self.inplace[k].is_null() { self.datums[k * ROWS..].as_ptr() as *const u8 } else { self.inplace[k] };
                Lane::byval(ptr, valid, self.recipe.widths[k], lane::REGION_ARENA, id)
            };
            l.kind = if self.lane_byref.has(k) { lane::KIND_BYREF } else { self.recipe.kinds[k] };
            l.ncodes_or_scale = self.lane_scale(self.cur_part, k);
            // M6 R3: a shred code lane the part did not elect is all-invalid: no dictionary, no codes
            if self.recipe.kinds[k] == lane::KIND_CODES && self.absent(self.cur_part, k) {
                l.region = lane::REGION_DICTIONARY;
                l.ncodes_or_scale = 0;
            } else if self.recipe.kinds[k] == lane::KIND_CODES {
                let entry = self.dict_at(self.cur_part, k).expect("dictionary");
                l.region = lane::REGION_DICTIONARY;
                l.ncodes_or_scale = entry.dict.ncodes();
                epoch = entry.dict.epoch64() as u32;
                if list.parts()[self.cur_part].deletes.is_some() {
                    l.flags |= lane::FLAG_DELETES;
                }
                if self.entries_staged && (self.recipe.entries == Some(k) || self.recipe.entries_every) {
                    l.flags |= lane::FLAG_ENTRIES;
                }
            }
            l.flags |= if self.sel_all { V_ALL } else { self.verdict };
            if self.lane_all.has(k) {
                l.flags |= lane::FLAG_LANE_ALL_MATCH;
            }
            if ck == Some(k) {
                l.aux = 1;
            }
            w.set(k, l);
        }
        if let Some(vk) = self.recipe.verdict_lane.filter(|_| self.cond_fp != [0, 0]) {
            let mut l = Lane::byval(self.cond_words.as_ptr() as *const u8, core::ptr::null(), 8, lane::REGION_ARENA, lane::id(lane::ID_VERDICT, 0));
            l.flags |= if self.cond_hit == V_ALL { V_ALL } else { V_MIXED };
            w.set(vk, l);
        }
        if self.selected {
            w.select(&self.sel);
        }
        w.rows = rows as u32;
        w.set_base(WindowBase { pos: ScanPos::new(self.recipe.rel, self.staged as u32, 0), part_id: self.cur_part as u32, dict_epoch: epoch, granule: (self.staged - list.starts()[self.cur_part]) as u32, claim_id: self.lo as u32 });
        // NEW-45: the code cut seeds a part's entry from its dictionary only where every entry stands for a delivered row
        if entries_safe(&list.parts()[self.cur_part], self.resolved_unbound()) {
            w.set_part_facts(gravity_abi::site::site::PART_ENTRIES_SAFE);
        }
        Ok(rows as u32)
    }

    /// The datum of dictionary code `code` of code lane `k` in part `p`: the part is opened
    /// (and stays with the Node) when this source never touched it; the pin is not kept.
    fn dict_value_pg(&mut self, p: usize, k: usize, code: u32) -> PgResult<(Word, u32)> {
        if p == lane::EMPTY_PART as usize {
            return Ok((EMPTY_IMAGE.as_ptr() as Word, 0));
        }
        // the list's length, not an Arc::clone of it: a render per row put every worker on its refcount line
        let nparts = self.list.as_ref().ok_or_else(|| err("gravity: dictionary lookup on an unbound source"))?.len();
        if p >= nparts || k >= self.recipe.natts || self.recipe.kinds[k] != lane::KIND_CODES {
            return Err(Box::new(PgError::error(format!("gravity: dictionary lookup outside the plan (part {p} of {nparts}, lane {k} of {}, kind {}, code {code})", self.recipe.natts, self.recipe.kinds.get(k).copied().unwrap_or(u8::MAX))))); // alloc: Cold
        }
        // D3823: a part-grain dictionary already in its cell renders from its handle, which owns its image and faults its
        // blocks through its own part: a render after the part's release (a HashAgg's code keys at the emit, a TopK's rows)
        // neither holds the part again to the statement's end nor opens a cursor; a verbatim part's render decodes its row
        // through this source's cursor, so it opens as before. One `dict_at` per render (`open_dict`'s open case inline):
        // a second, on every render of a verbatim part, cost a render-heavy grouped join 2.5 % of its instructions
        let open = self.cursors.get(p * self.recipe.natts + k).is_some_and(|c| c.is_some());
        if !self.dict_at(p, k).is_some_and(|e| open || matches!(e.dict, Dict::Part(_))) {
            cancelpoll::step("granule_source dict_value: a render's dictionary opened").tick().map_err(|_| err(RENDER_CANCELED))?;
            self.open_dict(p, k)?;
        }
        if pgrc_read::dict_census_on() {
            DICT_RENDERS.fetch_add(1, Ordering::Relaxed);
        }
        let face = self.face(k);
        let (epoch, ncodes, verbatim) = {
            let d = &self.dict_at(p, k).expect("dictionary").dict;
            (d.epoch64(), d.ncodes(), matches!(d, Dict::Ranked { .. }))
        };
        // D4327: a part's dictionary the statement released (`StmtDicts::release`) and this render opened again has the
        // same `epoch` (one per open part) but a new image: the memo is keyed by the release count too, so no pointer into
        // the released image is handed out (an emit's render never enters the part, where `part_enter` resets the memo)
        let opened = self.rel_unbound().stmt.as_deref().map_or(0, |sd| sd.epoch(p));
        // dict order 1: a fold renders every row, so a part's renders are memoised per code (a verbatim part's: its render table)
        // D4732 (c): a memo takes the held owner when it is made; a move of the held owner (`cursors_to`, at the bind's or a
        // window's re-home) reaches the memos made before it, so a render no longer asks for the owner each time
        if self.renders.len() <= k {
            let (made, owner) = (self.renders.len(), self.held.owner());
            self.renders.resize_with(k + 1, crate::dict::text_hosts::RenderMemo::new); // alloc: Node
            self.renders[made..].iter_mut().for_each(|m| m.set_owner(owner));
        }
        if let Some((d, n)) = self.renders[k].get(p, epoch, opened, ncodes, code, verbatim) {
            return Ok((d as Word, n));
        }
        // D3963: an emit's renders past the memo ran back to back in one kernel call, 140-430 ms each under load, with no poll
        cancelpoll::step("granule_source dict_value: a render past the memo").tick().map_err(|_| err(RENDER_CANCELED))?;
        let hit = match &self.dict_at(p, k).expect("dictionary").dict {
            Dict::Part(d) => {
                let e = d.entry(code).map_err(read_err)?;
                (e.image.as_ptr() as Word, e.byte_len)
            }
            // D3038 (A): a lane coded by length renders nothing (its codes rank lengths: text_hosts `lane_codes`)
            Dict::Ranked { table, .. } if table.ranks().is_some_and(|r| r.by_length().is_some()) => {
                return Err(err("gravity: Reason::Bug: D3038: a render of a code lane coded by length"));
            }
            Dict::Ranked { table, .. } if self.renders[k].walk_due() => {
                // a verbatim part's render decodes its code's row alone: its render table (one pass over the part's rows,
                // resident and shared) renders every code
                let _step = cancelpoll::step("granule_source dict_value: a verbatim part's render table");
                let table = table.arc_clone();
                let ranks = table.ranks().expect("ranks");
                let real = Arc::clone(self.parts().part(p, &self.recipe.registry)?);
                // FASTADD: a column the part lacks renders from its stand-in
                let part = Arc::clone(self.parts().lane(p, face, &real)?);
                let (attno, ord) = self.parts().list.faces.locate(face, p, &part)?.ok_or_else(|| err("gravity: a render of a lane the part did not elect"))?;
                let memo = self.fp_memo();
                let rendered = {
                    let read_err_cell = core::cell::Cell::new(None);
                    let cur_cell = core::cell::RefCell::new(self.cursors[p * self.recipe.natts + k].as_deref_mut().expect("cursor"));
                    let scratch_cell = core::cell::RefCell::new(&mut self.scratch);
                    let view = crate::dict::text_hosts::DictView::Ranked { ranks, cur: &cur_cell, scratch: &scratch_cell, err: &read_err_cell };
                    crate::dict::text_hosts::render_table_in(memo, &part, stream_key(attno, ord), &view).map_err(read_err)?
                };
                self.renders[k].take_table(rendered);
                if let Some((d, n)) = self.renders[k].get(p, epoch, opened, ncodes, code, true) {
                    return Ok((d as Word, n));
                }
                let (lg, i) = ranks.locator(code).ok_or_else(|| err("gravity: dictionary code outside the part's dictionary"))?;
                self.render_row(p, k, lg, i)?
            }
            Dict::Ranked { table, .. } => {
                let (lg, i) = table.ranks().expect("ranks").locator(code).ok_or_else(|| err("gravity: dictionary code outside the part's dictionary"))?;
                self.render_row(p, k, lg, i)?
            }
        };
        self.renders[k].put(code, hit);
        Ok(hit)
    }

    /// The cursors stay open (one list generation; `bind` drops them on a new one), but those over a part the statement
    /// released (D3823 `close_released`); the statement's `Resolved` is forgotten.
    fn unbind_pg(&mut self) {
        // SCANSTATS: this bind's granule census, a plain add at its bind's statement entry (landed with the statement's
        // other binds at its end)
        pgrc_am::scanstats::granule_scan_end(self.ss_at, self.recipe.rel, [self.granules_decoded, self.granules_skipped, self.granules_answered]);
        // a claim a stop left unfinished (a LIMIT) flushes here, before the statement's census reads the statics
        self.tally.flush();
        self.close_released();
        self.decay_cursors();
        self.prune_held();
        // C8: the cursors kept open pass their part uses to the backend's session (they outlive the statement); D3994
        // (B.1 W): and let their images go (`StreamCursor::park_images`), so the parked source pins no part bytes
        if self.touch_live {
            // D4554 (`kept_parked`): only the parts this statement opened moved or loaded anything; a parked cursor is no
            // live reader of its part (D4351) until a statement opens the part again. A worker slot's source leaves their
            // uses on the statement, parked, for the depot park's one move (else `settle_kept`'s to the session); a Node's
            // own source never parks in the depot, so it moves them to the session here, once
            let defer = self.slot_bound;
            self.each_kept_of(KeptParts::Touched, |g, c| {
                if defer {
                    c.keep_pending_park_in(g);
                } else {
                    c.keep_past_statement_in(g);
                }
                c.set_pass_behind(false);
            });
            self.park_pending |= defer && !self.touched.is_empty();
            self.touch_live = false;
        } else {
            self.each_kept_by_part(|g, c| c.keep_past_statement_in(g));
        }
        // D4554: and their pooled tables to the session, once
        if let Some(pool) = &self.kept_pool {
            pool.park();
        }
        // D3994 (B.1 W): the lanes' in-place pointers into a cursor's extent image go with it (`stage` nulls them before any
        // read; none may outlive the image the cursor no longer holds)
        self.inplace.fill(core::ptr::null());
        // D2856 (colskip): a source that took a runtime filter this bind leaves its window to the site's next bind. D4656:
        // and lets go of the filters it took. A parked source (kept warm past its statement) held them until its next bind,
        // past the statement's end, where a filter's runs (charged to its build's operator owner, D4533) were an operator
        // residue; now the execution's last rt claim frees them before that end. The next bind takes the cell's again.
        if self.rt_gen != u64::MAX {
            for k in 0..RT_SLOTS {
                self.cell.set_rt_window_at(k, self.rt[k].window());
                self.rt[k].f = None;
            }
            self.rt_gen = u64::MAX;
        }
        self.bound = false;
        self.pin = None;
        self.stats_read = (usize::MAX, 0);
        self.list = None;
        self.order = None;
        self.resolved = core::ptr::null();
        self.tables.fmgr = None;
        self.tables.pinned.clear();
        // CONVERT wave 2: the statement's shared sweep tables (charged to it) go with it, not with the parked source
        self.tables.swept.clear();
        self.tables.part = usize::MAX;
        self.tables.adapt_lazy.clear();
        self.tables.adapt_hosted = 0;
        self.cur_part = usize::MAX;
        self.nrows = 0;
        self.cur.clear();
        self.cond_due = None;
        self.cond_entry = None;
        self.sparse_tab = None;
        self.sparse_dense = usize::MAX;
        self.table_dense = usize::MAX;
        self.renders.clear();
        self.end_runs();
    }
}

/// D4554 (`kept_parked`): uses an unbind left pending that no park or statement's end took (an error's teardown) go to
/// the session before they drop.
impl Drop for GranuleRangeSource {
    fn drop(&mut self) {
        self.settle_pending();
    }
}

impl GranuleRangeSource {
    /// cvpread's read owner (`StreamCursor::set_owner`): this source's open cursors read (prefetch, uncached images) for
    /// the owner `held` moved to.
    fn cursors_to(&mut self, to: HeldOwner) {
        // D4554 (`kept_parked`): the parts the statement opened so far; a later one takes the owner at its open
        let touched = self.touch_live;
        self.each_held_of(touched, |c| c.set_owner(to));
        // D4732 (c): the render memos' bytes move with it (a memo made later takes it at its making, `dict_value_pg`)
        self.renders.iter_mut().for_each(|m| m.set_owner(to));
        // D4554: their pooled tables once (a pooled cursor's `set_owner` names its read owner only)
        if let Some(pool) = self.kept_pool.as_ref().filter(|_| to.is_some()) {
            pool.home(to);
        }
    }

    /// D4554: `f` over each kept cursor (decode and character-length), a part's cursors in one group of part-use moves
    /// (`pgrc_read::UseMoves`: the part's user list locked and its parked count moved once a part, not once a cursor).
    fn each_kept_by_part(&mut self, f: impl FnMut(&mut pgrc_read::UseMoves<'_>, &mut StreamCursor<'static>)) {
        self.each_kept_of(KeptParts::All, f);
    }

    /// D4554 (`kept_parked`): the uses its unbinds left pending (`keep_pending_park_in`) that no park took go to the
    /// session. Unbound only.
    fn settle_pending(&mut self) {
        // a bound source's uses are its running statement's; its drop frees them there
        if !self.park_pending || self.bound {
            return;
        }
        self.park_pending = false;
        // one bind since (a Node's own source each statement): its `touched` list, not a pass over every held part
        let parts = if self.binds == self.settle_bind + 1 { KeptParts::Touched } else { KeptParts::Since(self.settle_bind) };
        self.each_kept_of(parts, |g, c| c.keep_past_statement_in(g));
        self.settle_bind = self.binds;
    }

    /// D4554: `each_kept_by_part` over the parts `parts` names (`KeptParts`).
    fn each_kept_of(&mut self, parts: KeptParts, mut f: impl FnMut(&mut pgrc_read::UseMoves<'_>, &mut StreamCursor<'static>)) {
        let natts = self.recipe.natts.max(1);
        let touched = matches!(parts, KeptParts::Touched);
        let n = if touched { self.touched.len() } else { self.held_parts.len() };
        // bounded("key-columns: one group a part this source keeps cursors over, one O(1) move per kept cursor")
        for i in 0..n {
            let q = if touched { self.touched[i] } else { self.held_parts[i] } as usize;
            // a part no bind opened since bind `b` (its cursors' uses are where that park left them)
            if let KeptParts::Since(b) = parts {
                if self.part_bind.get(q).is_some_and(|&opened| opened <= b) {
                    continue;
                }
            }
            let (lo, hi) = (q * natts, (q + 1) * natts);
            let (nc, ns) = (self.cursors.len(), self.char_src.len());
            let cs = &mut self.cursors[lo.min(nc)..hi.min(nc)];
            let ss = &mut self.char_src[lo.min(ns)..hi.min(ns)];
            let mut it = cs.iter_mut().chain(ss.iter_mut()).flatten();
            let Some(first) = it.next() else { continue };
            let part = first.part().clone();
            let mut g = pgrc_read::UseMoves::new(&part);
            f(&mut g, first);
            // bounded("key-columns: one part's cursors, two per read column at most")
            for c in it {
                if g.is_of(c.part()) {
                    f(&mut g, c);
                } else {
                    let other = c.part().clone();
                    f(&mut pgrc_read::UseMoves::new(&other), c);
                }
            }
        }
    }

    /// D4554: `f` over each cursor (decode and character-length) of the parts this source holds cursors over.
    fn each_held(&mut self, f: impl FnMut(&mut StreamCursor<'static>)) {
        self.each_held_of(false, f);
    }

    /// D4554: `each_held` over the parts this bind opened (`touched`) or every part this source holds cursors over.
    fn each_held_of(&mut self, touched: bool, mut f: impl FnMut(&mut StreamCursor<'static>)) {
        let natts = self.recipe.natts.max(1);
        let (cursors, chars) = (&mut self.cursors, &mut self.char_src);
        let parts = if touched { &self.touched } else { &self.held_parts };
        // bounded("key-columns: one O(1) step per kept cursor, one per read column of a part this source holds")
        for &q in parts {
            let (lo, hi) = (q as usize * natts, (q as usize + 1) * natts);
            let (nc, ns) = (cursors.len(), chars.len());
            // bounded("key-columns: one part's cursors, two per read column at most")
            for c in cursors[lo.min(nc)..hi.min(nc)].iter_mut().chain(chars[lo.min(ns)..hi.min(ns)].iter_mut()).flatten() {
                f(c);
            }
        }
    }

    /// D4554: under the `kept_pool` switch, a cursor this source keeps charges its own tables to the source's pool (made at
    /// the first, on the owner the source's held bytes are on).
    fn pool_cursor(&mut self, cur: &mut StreamCursor<'static>) {
        if !mempolicy::keep::kept_pool() && !mempolicy::keep::kept_parked() {
            return;
        }
        let held = self.held.owner();
        let pool = self.kept_pool.get_or_insert_with(|| {
            let p = pgrc_read::KeptPool::shared();
            p.home(held);
            p
        });
        cur.set_pool(pool.clone());
    }

    /// D4464: a parked source (`Source::park`: its Node into the backend's park, its worker set into the slot depot) keeps
    /// at most `SCRATCH_KEEP` bytes of each bind-sized lane, selection and image buffer, not the last statement's high
    /// water; `bind_pg` sizes them again. The late lanes go as one group (`bind_pg` regrows them on a short `late_at`).
    fn park_scratch(&mut self) {
        if self.bound {
            return;
        }
        shed(&mut self.datums);
        // bounded("key-columns: one image arena per read column")
        for a in self.arenas.iter_mut() {
            shed(a);
        }
        shed(&mut self.valid);
        shed(&mut self.sel);
        shed(&mut self.sel16);
        shed(&mut self.dense);
        shed(&mut self.guard_sel);
        self.render = Vec::new(); // alloc: Node
        let late = [gravity_driver::vec_bytes(&self.late_done), gravity_driver::vec_bytes(&self.late_sel), gravity_driver::vec_bytes(&self.late_sel16)].into_iter().sum::<usize>() + self.late_more.iter().flatten().map(gravity_driver::vec_bytes).sum::<usize>();
        if late > SCRATCH_KEEP {
            (self.late_done, self.late_at, self.late_more) = (Vec::new(), Vec::new(), Vec::new()); // alloc: Node
            (self.late_cnt, self.late_req, self.late_span, self.late_calls) = (Vec::new(), Vec::new(), Vec::new(), Vec::new()); // alloc: Node
            (self.late_sel, self.late_sel16) = (Vec::new(), Vec::new()); // alloc: Node
        }
        self.held.sync(self.held_bytes());
    }

    /// D4554: the bytes `park_scratch` frees now (`Source::park_bytes`), as it decides them: 0 while bound.
    fn scratch_bytes(&self) -> usize {
        if self.bound {
            return 0;
        }
        use gravity_driver::vec_bytes as v;
        let over = |n: usize| if n > SCRATCH_KEEP { n } else { 0 };
        let lanes = [v(&self.datums), v(&self.valid), v(&self.sel), v(&self.sel16), v(&self.dense), v(&self.guard_sel)].into_iter().map(over).sum::<usize>();
        let arenas = self.arenas.iter().map(|a| over(v(a))).sum::<usize>();
        let render = v(&self.render) + self.render.iter().map(v).sum::<usize>();
        let late_sel = [v(&self.late_done), v(&self.late_sel), v(&self.late_sel16)].into_iter().sum::<usize>() + self.late_more.iter().flatten().map(v).sum::<usize>();
        let late = if late_sel > SCRATCH_KEEP {
            v(&self.late_done) + v(&self.late_at) + v(&self.late_more) + self.late_more.iter().map(|m| v(m) + m.iter().map(v).sum::<usize>()).sum::<usize>()
                + v(&self.late_cnt) + v(&self.late_req) + v(&self.late_span) + v(&self.late_calls) + v(&self.late_sel) + v(&self.late_sel16)
        } else {
            0
        };
        lanes + arenas + render + late
    }

    /// CONVERT wave 2: the bytes `held` charges: `kept_bytes` but the renders' memos (text_hosts charges its own).
    fn held_bytes(&self) -> u64 {
        (Source::kept_bytes(self) - self.renders.iter().map(crate::dict::text_hosts::RenderMemo::kept_bytes).sum::<usize>()) as u64
    }

    /// X5: a `FLAG_BOUND` code set's spec with its values read from the site's key cells (`bound_keys`): `Eq` over the
    /// constants and those values (NEW-72 entry C: a bound text constant's too). PS1-15: a `FLAG_BOUND` LIKE's: `Like`
    /// over the pattern its constant runs and those values assemble. NEW-72 entry C: a `FLAG_BOUND` byte interval's:
    /// `Range` with its value at the bound end. Either is `Faults` when a value is NULL, outside the cells, or not an inline text image (every
    /// entry a fault: each row evaluates the body, PG's answer; a NULL that makes the root NULL on every row, a LIKE's or
    /// an all-parameter code set's, the row reads as NULL without the table, gravity_expr emit `bound_null`). Any other
    /// spec is returned as it is.
    pub(super) fn bound_spec(&self, spec: crate::dict::text_hosts::TableSpec) -> crate::dict::text_hosts::TableSpec {
        use crate::dict::text_hosts::{TableFn, TableSpec};
        let read = |cells: &[u16]| cells.iter().map(|&c| raw::key_cell(self.bound_keys, c as usize).filter(|k| k.1 == 0).and_then(|k| raw::bind_text(k.0))).collect::<Option<Vec<_>>>(); // alloc: Node
        let func = match &spec.func {
            // P5-02: a whole list's cells, every element of each array a value
            TableFn::EqBound { values, cells, negated, list: true } => cells
                .iter()
                .map(|&c| raw::key_cell(self.bound_keys, c as usize).filter(|k| k.1 == 0).and_then(|k| raw::bind_text_list(k.0)))
                .collect::<Option<Vec<_>>>() // alloc: Node
                .map(|v| TableFn::Eq { values: values.iter().cloned().chain(v.into_iter().flatten()).collect(), negated: *negated }), // alloc: Node
            TableFn::EqBound { values, cells, negated, list: false } => read(cells).map(|v| TableFn::Eq { values: values.iter().cloned().chain(v).collect(), negated: *negated }), // alloc: Node
            // NEW-72 entry C: a byte interval's bound end
            TableFn::RangeBound { cells, below, nul } => read(cells).and_then(|v| v.into_iter().next()).map(|mut c| {
                if *nul {
                    c.push(0);
                }
                if *below { TableFn::Range { from: Vec::new(), to: Some(c) } } else { TableFn::Range { from: c, to: None } } // alloc: Node
            }),
            TableFn::LikeBound { parts, cells, ilike, negated } => read(cells).map(|v| {
                let mut pattern = parts[0].clone(); // alloc: Node
                // bounded("other: the pieces of one LIKE pattern of the statement's own quals, joined once per bind: the query text bounds them, as PG's like_fixed_prefix reads the same pattern with no poll")
                for (value, part) in v.iter().zip(&parts[1..]) {
                    pattern.extend_from_slice(value);
                    pattern.extend_from_slice(part);
                }
                TableFn::Like { pattern, ilike: *ilike, negated: *negated }
            }),
            _ => return spec,
        };
        TableSpec { over: spec.over, func: func.unwrap_or(TableFn::Faults) }
    }

}

impl Source for GranuleRangeSource {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        self.bind_pg(b).map_err(crate::stash)?;
        if self.held.home(HeldCharge::of_statement()) {
            self.cursors_to(self.held.owner());
        }
        self.held.sync(self.held_bytes());
        Ok(())
    }

    fn seek(&mut self, keys: &[Word], keys_null: &[u8]) -> Result<()> {
        if !self.bound {
            return Err(Error::host("seek on an unbound granule source"));
        }
        self.tally.flush();
        self.pin = None;
        self.cut = None;
        self.nrows = 0;
        self.cur.clear();
        self.cond_due = None;
        self.order = self.resolved_unbound().order_of(self.site);
        // TABLESAMPLE and ctid clauses over pgrc are refused (lowerv4 ops/scan/pgrc/lower.rs `make_with` refuses them):
        // pgrc's ctid is the row's stable ID (pgrc_format rowid IX0, kept through compaction), not its position, so a host
        // sampler or tid walk over positions would pick other rows
        if self.recipe.sample.is_some() {
            return Err(Error::host("gravity: TABLESAMPLE over a pgrc relation is not served (its ctid is the row's ID, not its position)"));
        }
        // X5: the bound code sets' cells come after every other cell (lowerv4 dict/percode.rs `attach`), so the cell count
        // below is the site's own protocol's without them
        self.bound_keys = (keys.as_ptr() as usize, keys_null.as_ptr() as usize, keys.len().min(keys_null.len()));
        // PS1-05: the open-time zone cells come after the source's own (`zone_keys`), never one of the counts below
        let bound = self.recipe.tables.iter().map(|t| match &t.func { crate::dict::text_hosts::TableFn::EqBound { cells, .. } | crate::dict::text_hosts::TableFn::LikeBound { cells, .. } | crate::dict::text_hosts::TableFn::RangeBound { cells, .. } => cells.len(), _ => 0 }).sum::<usize>();
        let nk = keys.len().saturating_sub(bound + self.recipe.zone_keys as usize);
        self.key_zone_seek(keys, keys_null);
        let total = self.total_granules();
        if keys_null[0] != 0 || keys_null[1] != 0 {
            self.hi = 0;
            self.g = 0;
            return Ok(());
        }
        let total = match &self.order {
            Some(o) => o.len() as u64,
            None => total,
        };
        self.hi = keys[1].min(total);
        self.lo = keys[0].min(self.hi);
        // a range site with two more key cells and no sampler: a granule range of leading ctid clauses by position
        // (lowerv4 ops/scan/pgrc/lower.rs `tid_keys`), refused over pgrc (above): never narrowed here
        if matches!(nk, 4 | 5) && self.order.is_none() {
            return Err(Error::host("gravity: a ctid clause over a pgrc relation is not served (its ctid is the row's ID, not its position)"));
        }
        self.g = self.lo;
        self.credit_from = self.lo;
        self.credit_part = usize::MAX;
        // a serial seek over the whole relation (no claims: the runner's census never cut it) takes the census itself
        self.dead = if self.order.is_none() && self.nzone > 0 && self.lo == 0 && self.hi == total {
            if self.dead_memo.is_none() {
                // the footer's zone maps are the columns' (never a jsonb column's): a shred lane's predicates stay out of it
                let mut cz = crate::ops::scan::heap_source::charged_scratch::<ZonePred>(self.held.owner());
                cz.with_vec(|v| v.extend(self.zone[..self.nzone].iter().filter(|z| self.recipe.shred_of(z.lane as usize).is_none()).copied())); // alloc: Node charged self.held.owner()
                self.dead_memo = Some(if cz.is_empty() { None } else { self.list.as_ref().and_then(|l| zone_dead(l, self.recipe.rel, &self.recipe.attnos[..self.recipe.natts], &cz)) });
            }
            self.dead_memo.clone().flatten()
        } else {
            None
        };
        Ok(())
    }

    fn zone(&mut self, preds: &[ZonePred], filter_lanes: &[u64], answer_lanes: &[u64]) {
        self.nzone_bind = preds.len();
        // a shred lane's faces are its path's (`Faces::locate`: the Stats / bloom / PSMA sealed at its `path_ord`, R13-L04),
        // so it takes zone predicates, elision and answers as a column does; the catalog, which holds its column's
        // records and no zone map of a jsonb column, never speaks for it (`face_record`, `zone_dead`, `answered_whole`)
        self.zone.clear();
        self.part_dec_p = usize::MAX;
        self.zone.extend_from_slice(preds); // alloc: Node
        let n = preds.len();
        // NEW-72: a bound predicate has no value until a bind writes one
        self.has_bound = preds.iter().any(|z| z.bound);
        self.bound_ok = !self.has_bound;
        self.nzone = if self.bound_ok { n } else { 0 };
        self.nzone_decl = self.nzone;
        self.nzone_static = n;
        self.filter_lanes.assign(&LaneBits::from_words(self.recipe.natts, filter_lanes));
        self.answer_lanes.assign(&LaneBits::from_words(self.recipe.natts, answer_lanes));
        self.zone_lanes.assign(&self.answer_lanes);
        // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
        for z in self.zone[..n].iter() {
            self.zone_lanes.set(z.lane as usize);
        }
        self.any_lanes();
        let lanes = &self.answer_lanes;
        self.tallied_mask = self.zone[..n].iter().enumerate().filter(|(i, z)| *i < 63 && z.tallied && lanes.has(z.lane as usize)).fold(0, |m, (i, _)| m | pred_bit(i));
    }

    fn zone_any(&mut self, preds: &[ZonePred], ends: &[u16], base: usize) {
        self.any.clear();
        self.any_ends.clear();
        if ends.last().is_some_and(|&e| e as usize == preds.len()) && preds.iter().all(|z| (z.lane as usize) < self.recipe.natts) {
            self.any.extend_from_slice(preds); // alloc: Node
            self.any_ends.extend_from_slice(ends); // alloc: Node
        }
        self.any_base = base;
        self.any_ok = !self.any.iter().any(|z| z.bound);
        self.any_lanes();
    }

    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        if self.held.home(HeldCharge::owner_of(w)) {
            self.cursors_to(self.held.owner());
        }
        self.scratch.set_owner(self.held.owner());
        let r = self.window_pg(w).map_err(crate::stash);
        self.held.sync(self.held_bytes());
        r
    }

    fn dict_value(&mut self, part_id: u32, lane: u16, code: u32) -> Result<(Word, u32)> {
        self.dict_value_pg(part_id as usize, lane as usize, code).map_err(crate::stash)
    }

    fn lane(&mut self, k: u16, sel: &[u32], out: &mut Lane) -> Result<u32> {
        self.lane_pg(k as usize, sel, out).map_err(crate::stash)
    }

    fn cursor(&self) -> &Cursor {
        &self.cur
    }

    fn cursor_mut(&mut self) -> &mut Cursor {
        &mut self.cur
    }

    fn lanes_hint(&self) -> usize {
        (self.recipe.natts + 1 + self.recipe.tables.len()).max(self.recipe.verdict_lane.map_or(0, |k| k + 1))
    }

    fn refetch(&mut self, _values: &mut [Word], _isnull: &mut [u8]) -> Result<bool> {
        Err(crate::unsupported())
    }

    fn substitute(&mut self, _pos: ScanPos) {}

    fn unbind(&mut self) {
        self.unbind_pg();
        debug_assert_eq!(self.cursors_held, self.cursors.iter().chain(self.char_src.iter()).flatten().count() * CURSOR_BYTES, "the boxed cursors' count");
        debug_assert!(
            (0..self.cursors.len().max(self.char_src.len()).div_ceil(self.recipe.natts.max(1))).all(|q| !self.holds_part(q) || self.held_mark.get(q).copied().unwrap_or(false) && self.held_parts.contains(&(q as u32))),
            "D4554: a part with a cursor open is in held_parts"
        );
        self.held.park(self.held_bytes());
    }

    /// D4630 (b): its bind reads the binding and its own state alone (the part list's shared faces are once-cells), keeps
    /// no pin, and homes its kept cursors' uses on the claim's owner (`memledger::charge::context`): a worker slot may bind
    /// it at its first claim (`slots::bind_at_claim`)
    fn binds_at_claim(&self) -> bool {
        true
    }

    /// D3196 (rearm E1): a warm worker slot's next section: the runs its parts published for the last section's owner phase
    /// dropped and its tally landed, as the unbind did after every section (a section's owners read its own runs alone).
    fn rearm(&mut self) -> Result<()> {
        self.tally.flush();
        self.end_runs();
        Ok(())
    }

    fn pins(&self) -> u64 {
        self.pin.is_some() as u64
    }

    /// skewsplit (D3320): the claim's driving site adopts the cut when it reads the claim as handed out in plain granule
    /// order (no walk order, no sampler, no TID range narrowing it); any other read of the claim forbids further cuts and
    /// stops at a cut end already moved (every range site of the claim reads its units, so each stops at the same end).
    fn claim_cut(&mut self, cut: &gravity_driver::claimcut::Cut, drive: bool) -> Result<()> {
        if drive && self.cut.is_none() && self.order.is_none() && self.recipe.sample.is_none() && (self.lo, self.hi) == cut.claim() {
            if let Some(token) = cut.adopt() {
                self.cut = Some(token);
                self.cut_skip0 = self.granules_skipped;
                return Ok(());
            }
        }
        cut.forbid();
        if cut.was_cut() {
            self.hi = self.hi.min(cut.end());
            self.lo = self.lo.min(self.hi);
            self.g = self.g.min(self.hi);
        }
        Ok(())
    }

    fn release(&mut self) {
        self.pin = None;
        self.cut = None;
        self.cur.clear();
        // D3115: a claim that ended mid-sweep (an error, a cancel) leaves its chunk to the sweep's other sources
        self.tables.abandon_sweeps();
    }

    /// D1507: an unbound source's kept cursors and pin (each holds its part file open) moved out; the next bind reopens
    /// lazily (the generation reset makes it drop what is left). A bound source keeps them (its statement reads them).
    fn release_rel(&mut self, rel: u32) -> Option<Box<dyn core::any::Any>> {
        if self.bound || (rel != 0 && rel != self.recipe.rel) {
            return None;
        }
        let kept: Vec<Box<StreamCursor<'static>>> = self.cursors.iter_mut().filter_map(Option::take).collect(); // alloc: Cold
        self.cursors_held = 0;
        let pin = self.pin.take();
        self.lanes_open.fill(false);
        self.octet_memo = None;
        self.char_src.clear();
        self.clear_held();
        self.cursor_generation = 0;
        self.held.sync(self.held_bytes());
        (!kept.is_empty() || pin.is_some()).then(|| Box::new((kept, pin)) as Box<dyn core::any::Any>) // alloc: Cold
    }

    /// D4768: the relation of its recipe.
    fn rel_read(&self) -> u32 {
        self.recipe.rel
    }

    /// The slot depot's hook: the bind-sized lanes, selections, tables and scratch this source keeps (`held`) to `to` (the
    /// depot's holder at a park, the taking statement at a take), and its kept cursors' own tables with them (cvpread's
    /// `transfer_kept`; their part uses stay where `kept_uses_to` put them: `Cache(Parked)`).
    fn transfer_held(&mut self, to: memledger::Owner, release: memledger::Release) {
        if self.bound {
            return;
        }
        self.held.transfer(to, release);
        // D4554: pooled, their tables move once
        if let Some(pool) = &self.kept_pool {
            pool.transfer(to, release);
        }
        // D4554 (`kept_parked`): every kept cursor is pooled (`pool_cursor`): no cursor of its own to move
        if !mempolicy::keep::kept_parked() {
            self.each_held(|c| c.transfer_kept(to, release));
        }
    }

    /// C8 (D4087): the kept decode cursors' and character-length cursors' part uses to `to` (a parked worker set's: the
    /// depot's).
    fn kept_uses_to(&mut self, to: memledger::Owner, release: memledger::Release) {
        if self.bound {
            return;
        }
        // D4554 (`kept_parked`): a statement reads through the kept cursors with their uses left on the depot; the uses an
        // unbind left pending (or a settle moved to the session) are those of the cursors a bind opened since the last park
        // (several binds: a worker slot binds again at each section of a statement), so the park walks the parts those
        // binds opened
        let parts = if mempolicy::keep::kept_parked() { KeptParts::Since(self.park_bind) } else { KeptParts::All };
        self.each_kept_of(parts, |g, c| c.transfer_use_in(g, to, release));
        self.park_bind = self.binds;
        self.settle_bind = self.binds;
        self.park_pending = false;
        // D4554: their pooled tables follow the uses, once
        if let Some(pool) = &self.kept_pool {
            pool.home(Some((to, memledger::Kind::Region)));
        }
    }

    fn settle_kept(&mut self) {
        self.settle_pending();
    }

    fn park(&mut self) {
        self.park_scratch();
    }

    fn park_bytes(&self) -> usize {
        self.scratch_bytes()
    }

    /// D3213: the bind-sized lanes, selections and scratch, kept at their high water while the source runs (D4464: a
    /// parked one keeps at most `SCRATCH_KEEP` bytes of each, `park_scratch`; natts x (ROWS + PF_WORDS) datum words alone
    /// are 64 KiB a column).
    fn kept_bytes(&self) -> usize {
        use gravity_driver::vec_bytes as v;
        let fixed = core::mem::size_of::<[u64; VALIDITY_WORDS]>() * 2 + core::mem::size_of::<[u64; DV_GRANULE_WORDS]>();
        let nested = self.arenas.iter().map(v).sum::<usize>() + self.late_more.iter().map(|m| v(m) + m.iter().map(v).sum::<usize>()).sum::<usize>() + self.render.iter().map(v).sum::<usize>();
        let lanes = v(&self.cursors) + v(&self.seen_epoch) + v(&self.lanes_open) + v(&self.part_bind) + v(&self.held_mark) + v(&self.held_parts) + v(&self.touched) + v(&self.part_dec) + v(&self.faces) + v(&self.datums) + v(&self.arenas) + v(&self.inplace) + v(&self.valid) + v(&self.sel) + v(&self.sel16) + v(&self.dense) + v(&self.all_valid);
        let zones = v(&self.zone) + v(&self.any) + v(&self.any_ends) + v(&self.zcode_images) + v(&self.zcode_preds) + v(&self.zlen) + v(&self.key_zone) + self.key_vals.as_ref().map_or(0, v) + v(&self.map_cols);
        let late = v(&self.late_done) + v(&self.late_at) + v(&self.late_more) + v(&self.late_sel) + v(&self.guard_sel) + v(&self.late_sel16) + v(&self.late_cnt) + v(&self.late_req) + v(&self.late_span) + v(&self.late_calls);
        let rest = v(&self.claim_runs) + v(&self.claim_miss_runs) + v(&self.runs_seen) + v(&self.nulls_seen) + v(&self.answers) + v(&self.octet_lens) + v(&self.render) + v(&self.renders) + self.renders.iter().map(crate::dict::text_hosts::RenderMemo::kept_bytes).sum::<usize>();
        let reads = v(&self.char_src) + v(&self.char_lens) + v(&self.octet_words) + v(&self.ahead) + v(&self.ahead_runs) + v(&self.enc) + self.runs.kept_bytes();
        // D4631: the lane sets and its own recipe copy, built with the source (a worker slot's build on whichever backend
        // made the slot) and kept for its life: on its charge, so whatever backend drops a parked slot takes them back first
        let sets = [&self.lane_byref, &self.filter_lanes, &self.answer_lanes, &self.zone_lanes, &self.record_lanes, &self.mapped, &self.map_all, &self.meta_need, &self.lane_all, &self.elided, &self.first, &self.rest, &self.enc_set, &self.scratch_a, &self.scratch_b];
        let own = sets.iter().map(|b| b.heap_bytes()).sum::<usize>() + match &self.recipe {
            RecipeHome::Own(r) => r.heap_bytes(),
            RecipeHome::Lent(_) => 0,
        };
        fixed + nested + lanes + zones + late + rest + reads + own + self.cursors_held + self.tables.kept_bytes() + self.scratch.kept_bytes()
    }

    /// Bound: the statement's open parts of the relation; unbound: the parts this source's kept cursors hold.
    fn residency(&self) -> Residency {
        if !self.bound {
            return Residency { parts: self.lanes_open.iter().filter(|o| **o).count() as u32, bytes: 0 };
        }
        self.parts().residency()
    }

    /// Road dict.entries' runs: accepted for this site's entries window over lane 0 whose one table is that lane's own
    /// `Fp128` (the consumer images each entry by it) when the consumer's table is the plain Fp128 pair (entry_runs.rs
    /// `accepts`) and the program filters no entry (`entries_filtered`: runs would skip the filter, PS1-18); anything else
    /// keeps every part's rows.
    fn entry_runs_offer(&mut self, layout: &gravity_abi::breaker::RecordLayout, bits: u8) {
        let own_fp = matches!(&self.recipe.tables[..], [t] if t.over == 0 && t.func == crate::dict::text_hosts::TableFn::Fingerprint);
        self.runs_ok = self.bound && self.recipe.entries == Some(0) && !self.recipe.entries_filtered && !self.recipe.entries_nulls && self.recipe.natts == 1 && own_fp && self.order.is_none() && crate::ops::scan::pgrc::entry_runs::accepts(layout, bits);
    }

    fn owner_input(&self, pbits: u8) -> Option<&dyn gravity_driver::SpillFile> {
        self.runs.at(pbits)
    }

    fn has_owner_input(&self) -> bool {
        !self.runs.is_empty()
    }
}

