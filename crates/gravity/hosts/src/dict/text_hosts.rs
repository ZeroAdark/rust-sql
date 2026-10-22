//! The text-lane face (hosts.md "The verbatim part's code lane"): a
//! text column is a CODE lane in every window — a dictionary-coded part publishes the writer's codes,
//! a verbatim part the byte-ranked codes of its distinct values ([`Ranks`], a property of the part
//! cached with its `Fp128` table); kernels, the breaker record and the leader's render see one lane kind.

mod rank;
mod raw;
pub use raw::BuildBuf;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Weak};

use pgsync::{Mutex, OnceLock};

use gravity_fn::arena::{AVec, ForgetSafe, OpArena, Refused};
use pgrc_format::abi::{ByteArena, DecodeOut, ValidityVerdict};
use pgrc_format::wire::varlena_header_4b_u;
use pgrc_format::FormatError;
use pgrc_read::{DictHandle, OpenPart, ReadError, StreamCursor};
use types_error::PgResult;
use types_rel::Relation;

/// A column the face serves: codec facts `dict` (every part) or `text` (verbatim in some part).
pub fn is_text_lane_column(rel: &Relation<'_>, attno: i16) -> PgResult<bool> {
    let Some(codecs) = pgrc_am::footer::footer_codecs(rel)? else { return Ok(false) };
    Ok(attno >= 1 && codecs.codec.get(attno as usize - 1).is_some_and(|&c| c == pgrc_am::footer::CODEC_DICT || c == pgrc_am::footer::CODEC_TEXT))
}

/// The committed granule count (sizes a per-granule run); `None` = no committed publish.
pub fn granule_count(rel: &Relation<'_>) -> PgResult<Option<u64>> {
    Ok(pgrc_am::footer::footer_codecs(rel)?.map(|c| c.granules))
}

/// The catalog's retirement face: a part that left the catalog drops its fingerprint tables now
/// (the part's `Arc` going away drops them at the next insert regardless).
pub fn retire_part(uuid: [u8; 16]) -> u64 {
    retire_fp(uuid)
}

/// A part's `Fp128` table resident process-wide (partitioned-record.md R2): the entry
/// fingerprints are a pure function of the immutable part's values and the process seed, so the
/// table is a property of the part like its reader-built dictionary — keyed `(uuid, attno, form)` beside the part's
/// entry runs and its condition tables (`cond_table`: the form is the spec's fingerprint and a hit compares the whole
/// spec, so part, column, body and constants), LRU by bytes under `fp_cache_cap`, dropped with the part. 16 B per
/// entry; the buffer is 2 MiB-page-advised and pre-faulted at the build (§4b (d), (f)) so no statement's loop takes
/// the faults.
const FP_CACHE_GUC: &str = "pgrust.gravity_fp_cache_mb";
static FP_CACHE: Mutex<Option<FpCache>> = Mutex::new(None);
static FP_BUILT: AtomicU64 = AtomicU64::new(0);
static FP_RETIRED: AtomicU64 = AtomicU64::new(0);

pub struct FpTable {
    /// G1-A + C8 move 7: the charge its bytes are on: its building owner's from the build (`table_charge`), the fp tables'
    /// from when the resident cache took it (None: neither), back on the dropping thread's account at its drop.
    cached: Mutex<Option<memledger::Charge>>,
    /// pagepool P3: a pool buffer's bytes are on its own charge, moved with `cached`'s owner (`BuildBuf::own`).
    buf: BuildBuf,
    hdr: *const gravity_dict::table::Header,
    /// A verbatim part's codes (`None` for a dictionary-coded part's table).
    ranks: Option<Ranks>,
    used: u64,
    /// A part's entry runs (granule_source/entry_runs.rs): `buf` holds its run records and these are its bucket offsets; no header.
    /// A render table's (`renders`): each code's word offset of its image in `buf`, `u32::MAX` for none.
    off: Option<Box<[u32]>>,
    /// A verbatim part's rendered images (`render_table`).
    renders: bool,
    /// PINS: who references the table once it is in the resident cache (`FpPin`), made at the first pin.
    pins: Mutex<Option<Arc<memledger::pins::PinSet>>>,
}

/// A scan's reference to a table of the resident cache, with its pin: the table's bytes are on the pinning portal's or
/// statement's `pins` while the reference lives, and on its `pins_evicted` once the cache evicted the table
/// (memledger::pins). The reference and the guard are one value: whoever keeps the table keeps the count.
pub struct FpPin {
    table: Arc<FpTable>,
    /// Declared after the reference it counts: it drops first or last, either way the counts follow the reference.
    _pin: Option<memledger::pins::PinGuard>,
}

impl FpPin {
    /// `table` referenced by the calling thread's portal or statement. A table the cache does not hold (its bytes are its
    /// builder's, in the builder's `held`) is not counted.
    pub fn of(table: &Arc<FpTable>) -> FpPin {
        FpPin { table: table.clone(), _pin: table.pin() }
    }

    /// The reference, for a callee that takes the `Arc` (it does not keep it).
    pub fn arc(&self) -> &Arc<FpTable> {
        &self.table
    }

    /// An owned reference to the table, uncounted (a callee that keeps it only for the call's length).
    pub fn arc_clone(&self) -> Arc<FpTable> {
        self.table.clone()
    }
}

impl core::ops::Deref for FpPin {
    type Target = FpTable;

    fn deref(&self) -> &FpTable {
        &self.table
    }
}

impl Clone for FpPin {
    /// A second reference is a second holder (the cloning thread's portal or statement).
    fn clone(&self) -> FpPin {
        FpPin::of(&self.table)
    }
}

/// A verbatim part's code lane, built once per part per process and cached with its `Fp128` table:
/// the distinct values in byte-rank order (code 0 = `''` when present, the writer's convention), one
/// code per row in granule order, and per code the global row index of one row holding it (the
/// render decodes that row alone; no image is kept).
/// D3156: a kept-granule table's (`Kept`) codes cover only its granules: the others hold no row (an empty run in
/// `starts`), and `kept` / `salt` name them.
pub struct Ranks {
    codes: Vec<u32>,
    starts: Vec<u32>,
    reps: Vec<u32>,
    empty: bool,
    kept: Option<Box<[u64]>>,
    salt: u64,
    /// D3038 (A): a table coded by length (`lane_codes`): code `c`'s byte length, the part's `c`-th distinct length
    /// ascending; `None` for a byte-ranked table.
    lens: Option<Box<[i64]>>,
    /// The words `render_table` lays the distinct values out in (a 4-byte header and the bytes, each rounded up to 8), so
    /// it allocates once (grown by doubling, every copy faulted fresh pages, most of a near-unique key's render cost); 0
    /// for a table coded by length (`lane_codes`), which no render reads.
    render_words: usize,
}

impl Ranks {
    /// D3038 (A): each code's byte length when the table is coded by length (`lane_codes`): its codes rank lengths, not
    /// bytes, so only an `OCTET_LENGTH` table and the emptiness test read them.
    pub fn by_length(&self) -> Option<&[i64]> {
        self.lens.as_deref()
    }

    pub fn ncodes(&self) -> u32 {
        self.reps.len() as u32
    }

    pub fn has_empty(&self) -> bool {
        self.empty
    }

    /// Granule `lg`'s codes; empty for a granule a kept-granule table leaves out (`covers`).
    pub fn granule_codes(&self, lg: u32) -> &[u32] {
        &self.codes[self.starts[lg as usize] as usize..self.starts[lg as usize + 1] as usize]
    }

    /// D3156: whether granule `lg`'s rows have codes here (every granule's, but a kept-granule table's).
    #[inline]
    pub fn covers(&self, lg: u32) -> bool {
        self.kept.as_deref().is_none_or(|k| k.get(lg as usize / 64).is_some_and(|w| w >> (lg % 64) & 1 != 0))
    }

    /// D3156: the kept-granule table's cache identity (`Kept::salt`); 0 for a whole part's.
    pub fn salt(&self) -> u64 {
        self.salt
    }

    /// The granules this table covers, in order.
    fn granules(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.starts.len() as u32 - 1).filter(|&lg| self.covers(lg))
    }

    /// `code`'s representative row as `(granule, row in granule)`.
    pub fn locator(&self, code: u32) -> Option<(u32, u32)> {
        let row = *self.reps.get(code as usize)?;
        let lg = self.starts.partition_point(|&s| s <= row) - 1;
        Some((lg as u32, row - self.starts[lg]))
    }

    fn bytes(&self) -> u64 {
        4 * (self.codes.len() + self.starts.len() + self.reps.len()) as u64 + 8 * (self.kept.as_ref().map_or(0, |k| k.len()) + self.lens.as_ref().map_or(0, |l| l.len())) as u64
    }

    /// D4529: what its lists allocate: `bytes` at their capacities (`reps` and `starts` grow by pushes, so up to half of
    /// each can be capacity past its length).
    fn alloc_bytes(&self) -> u64 {
        4 * (self.codes.capacity() + self.starts.capacity() + self.reps.capacity()) as u64 + 8 * (self.kept.as_ref().map_or(0, |k| k.len()) + self.lens.as_ref().map_or(0, |l| l.len())) as u64
    }
}

/// D3156: the granules of a verbatim part one statement's zone verdicts keep (bit `lg`), when they are a small share of the
/// part's: its code lane's table (`row_codes`) decodes, ranks and fingerprints only their rows. `salt` (never 0) keys the
/// table and every table built over it in the fingerprint cache (`ranks_key`), apart from the whole part's.
pub struct Kept {
    bits: Box<[u64]>,
    salt: u64,
}

impl Kept {
    /// The granules `kept` (bit `lg`) of a part of `granules` granules.
    pub fn new(bits: Box<[u64]>, granules: u32) -> Kept {
        // FNV-1a over the words and the count, never 0 (the whole part's salt)
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        // bounded("other: one word per 64 granules of one part (its granule count over 64, plus one), an 8-byte hash step each")
        for w in bits.iter().copied().chain([granules as u64]) {
            // bounded("const-domain: a word's eight bytes")
            for b in w.to_le_bytes() {
                h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
            }
        }
        Kept { bits, salt: h | 1 }
    }

    pub fn covers(&self, lg: u32) -> bool {
        self.bits.get(lg as usize / 64).is_some_and(|w| w >> (lg % 64) & 1 != 0)
    }

    pub fn count(&self) -> u32 {
        self.bits.iter().map(|w| w.count_ones()).sum()
    }

    pub fn salt(&self) -> u64 {
        self.salt
    }
}

/// D3156: a kept-granule table's attno bit in its cache keys (a stream key's ord stays under 2^15).
const KEPT_BIT: u32 = 1 << 31;

/// The fingerprint-cache key of a table over a verbatim part's `ranks` (`third`: its form): a kept-granule table's under
/// `KEPT_BIT` and its salt, so no statement with other kept granules (or the whole part) reads it.
fn ranks_key(part: &OpenPart, attno: u32, third: u64, ranks: Option<&Ranks>) -> ([u8; 16], u32, u64) {
    match ranks.map(Ranks::salt).filter(|&s| s != 0) {
        Some(s) => (part.uuid(), attno | KEPT_BIT, third ^ s),
        None => (part.uuid(), attno, third),
    }
}

/// The decode scratch of a pass over a part's rows (`row_codes`, `DictView::Ranked`): kept by the
/// caller so a statement's passes allocate nothing after the first.
pub struct TextScratch {
    datums: Vec<u64>, // alloc: Node
    arena: Vec<u64>,  // alloc: Node
    valid: Vec<u64>,  // alloc: Node
    /// A granule's value lengths (`decode_lengths`).
    lens: Vec<u32>, // alloc: Node
    /// D3288 (C): a decoded granule's row spans in the arena (`row_spans`).
    spans: Vec<(usize, usize)>, // alloc: Node
    /// The owner a build's working memory over a part's codes or rows is charged to (`set_owner`, `arena_of`).
    owner: BuildOwner,
}

/// The owner a dictionary build's working memory is charged to and the kind it carries there: the slot the source's
/// pipeline produces into with its kind (`Window::op_owner`), else the statement's (or the portal's) owner with
/// `Kind::Region`; none: charged to no owner, where an allocator `Vec`'s bytes sit.
pub type BuildOwner = Option<(memledger::Owner, memledger::Kind)>;

impl TextScratch {
    pub const fn new() -> TextScratch {
        TextScratch { datums: Vec::new(), arena: Vec::new(), valid: Vec::new(), lens: Vec::new(), spans: Vec::new(), owner: None } // alloc: Node
    }

    /// The owner the source's builds charge from now on (`BuildOwner`).
    pub fn set_owner(&mut self, owner: BuildOwner) {
        self.owner = owner;
    }

    pub fn owner(&self) -> BuildOwner {
        self.owner
    }

    /// D3213: the scratch a parked source keeps (`Source::kept_bytes`).
    pub fn kept_bytes(&self) -> usize {
        use gravity_driver::vec_bytes as v;
        v(&self.datums) + v(&self.arena) + v(&self.valid) + v(&self.lens) + v(&self.spans)
    }

    /// Granule `lg` of `cur` decoded: the rows, and whether every row is valid (else `valid` holds the words).
    fn decode(&mut self, cur: &mut StreamCursor<'static>, lg: u32) -> Result<(usize, bool), ReadError> {
        let rows = cur.rows_in_granule(lg) as usize;
        if self.datums.len() < rows {
            self.datums.resize(rows, 0); // alloc: Node
        }
        if self.valid.len() < rows.div_ceil(64) {
            self.valid.resize(rows.div_ceil(64), 0); // alloc: Node
        }
        if self.arena.is_empty() {
            self.arena.resize(1 << 13, 0); // alloc: Node
        }
        // bounded("other: a retry only when the arena was short, grown at least twofold and by what the decode asked for, so the second decode of the granule fits")
        loop {
            let bytes_len = self.arena.len() * 8;
            let bytes = raw::bytes_of(&mut self.arena);
            let mut out = DecodeOut { datums: &mut self.datums[..rows], arena: ByteArena::new(bytes) };
            match cur.decode_full(lg, &mut out) {
                Ok(n) if n as usize == rows => break,
                Ok(_) => return Err(ReadError::Format(FormatError::Corrupt { at: "text pass: decoded count" })),
                Err(ReadError::Format(FormatError::ArenaExhausted { needed })) => {
                    let want = (bytes_len + needed).max(bytes_len * 2);
                    self.arena.resize(want.div_ceil(8), 0); // alloc: Node
                }
                Err(e) => return Err(e),
            }
        }
        let all = self.validity(cur, lg, rows)?;
        Ok((rows, all))
    }

    /// Granule `lg`'s validity into `valid`: whether every row is valid.
    fn validity(&mut self, cur: &mut StreamCursor<'static>, lg: u32, rows: usize) -> Result<bool, ReadError> {
        if self.valid.len() < rows.div_ceil(64) {
            self.valid.resize(rows.div_ceil(64), 0); // alloc: Node
        }
        Ok(!cur.has_validity_stream() || matches!(cur.validity(lg, &mut self.valid[..rows.div_ceil(64)])?, ValidityVerdict::AllValid))
    }

    /// D3038: each code's payload byte length over a verbatim part's ranks into `out` (one zeroed word per code) — one
    /// pass over the part's rows like `DictView::Ranked`'s `entries`, but reading only each value's length
    /// (`decode_lengths`: entry headers, no payload copied or materialized); a granule the length decode refuses is decoded
    /// in full.
    fn ranked_lengths(&mut self, cur: &mut StreamCursor<'static>, ranks: &Ranks, out: &mut [i64]) -> Result<(), ReadError> {
        let mut step = cancelpoll::step("text_hosts ranked_lengths: a verbatim part's code lengths");
        for lg in ranks.granules() {
            step.tick()?;
            let rows = cur.rows_in_granule(lg) as usize;
            if self.lens.len() < rows {
                self.lens.resize(rows, 0); // alloc: Node
            }
            let codes = ranks.granule_codes(lg);
            match cur.decode_lengths(lg, &mut self.lens[..rows]) {
                Ok(n) if n as usize == rows => {
                    // every live row of a code holds the code's bytes, so its length is the code's: a plain store per
                    // row (sequential reads of `codes` / `lens`), no load of the code's representative row
                    let all = self.validity(cur, lg, rows)?;
                    let lens = &self.lens[..rows];
                    if all {
                        // bounded("small-fixed: one granule's rows (at most GRANULE_ROWS, 8192), a store each; the granule loop ticks")
                        for (&c, &l) in codes.iter().zip(lens) {
                            out[c as usize] = l as i64;
                        }
                    } else {
                        // bounded("small-fixed: one granule's rows (at most GRANULE_ROWS, 8192), a store each; the granule loop ticks")
                        for (i, (&c, &l)) in codes.iter().zip(lens).enumerate() {
                            if (self.valid[i / 64] >> (i % 64)) & 1 != 0 {
                                out[c as usize] = l as i64;
                            }
                        }
                    }
                }
                Ok(_) => return Err(ReadError::Format(FormatError::Corrupt { at: "text pass: length count" })),
                Err(ReadError::Unsupported { .. }) => {
                    let (rows, all) = self.decode(cur, lg)?;
                    // bounded("small-fixed: one granule's rows (at most GRANULE_ROWS, 8192), a store each; the granule loop ticks")
                    for i in 0..rows {
                        if self.live(all, i) {
                            out[codes[i] as usize] = raw::payload(self.datums[i]).len() as i64;
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// D3038: granule `lg`'s payload byte length of each row into `out` (`rows_in_granule(lg)` of them), a NULL row's 0;
    /// whether every row is valid (else `valid_words` holds them). A verbatim stream's from its entry headers
    /// (`decode_lengths`, no payload read); a stream the length decode refuses is decoded in full.
    pub fn granule_lengths(&mut self, cur: &mut StreamCursor<'static>, lg: u32, out: &mut [u32]) -> Result<bool, ReadError> {
        let rows = cur.rows_in_granule(lg) as usize;
        if out.len() < rows {
            return Err(ReadError::Format(FormatError::Bounds { at: "text pass: lengths out" }));
        }
        match cur.decode_lengths(lg, &mut out[..rows]) {
            Ok(n) if n as usize == rows => {
                let all = self.validity(cur, lg, rows)?;
                if !all {
                    // bounded("small-fixed: one granule's rows (at most GRANULE_ROWS, 8192), a store each")
                    for (i, l) in out[..rows].iter_mut().enumerate() {
                        if !self.live(false, i) {
                            *l = 0;
                        }
                    }
                }
                Ok(all)
            }
            Ok(_) => Err(ReadError::Format(FormatError::Corrupt { at: "text pass: length count" })),
            Err(ReadError::Unsupported { .. }) => {
                let (rows, all) = self.decode(cur, lg)?;
                // bounded("small-fixed: one granule's rows (at most GRANULE_ROWS, 8192), a store each")
                for (i, l) in out[..rows].iter_mut().enumerate() {
                    *l = if self.live(all, i) { raw::payload(self.datums[i]).len() as u32 } else { 0 };
                }
                Ok(all)
            }
            Err(e) => Err(e),
        }
    }

    /// D3038 (R3): granule `lg`'s character count of each row into `out` (`rows_in_granule(lg)` of them, UTF-8: the
    /// server encoding), a NULL row's 0; whether every row is valid (else `valid_words` holds them). The values are
    /// decoded.
    pub fn granule_chars(&mut self, cur: &mut StreamCursor<'static>, lg: u32, out: &mut [u32]) -> Result<bool, ReadError> {
        let (rows, all) = self.decode(cur, lg)?;
        if out.len() < rows {
            return Err(ReadError::Format(FormatError::Bounds { at: "text pass: chars out" }));
        }
        // bounded("small-fixed: one granule's rows (at most GRANULE_ROWS, 8192), a character count over each value's bytes, as PG's length() counts them with no poll")
        for (i, l) in out[..rows].iter_mut().enumerate() {
            *l = if self.live(all, i) { utf8_chars(raw::payload(self.datums[i])) as u32 } else { 0 };
        }
        Ok(all)
    }

    /// The validity words of the last granule `granule_lengths` (or a decode) read when it was not all valid.
    pub fn valid_words(&self) -> &[u64] {
        &self.valid
    }

    #[inline(always)]
    fn live(&self, all: bool, i: usize) -> bool {
        all || (self.valid[i / 64] >> (i % 64)) & 1 != 0
    }

    /// D3288 (C): the decoded granule's `rows` as `spans` (each live row's value bytes from the arena's start, a NULL
    /// row an empty span at the bytes so far) and the arena bytes they lie in; `None` when the images do not lie
    /// ascending in the arena.
    fn row_spans(&mut self, rows: usize, all: bool) -> Option<usize> {
        let base = self.arena.as_ptr() as usize;
        let limit = self.arena.len() * 8;
        self.spans.clear();
        let mut end = 0usize;
        // bounded("small-fixed: one granule's rows (at most GRANULE_ROWS, 8192), a span each")
        for i in 0..rows {
            if !self.live(all, i) {
                self.spans.push((end, 0)); // alloc: Node
                continue;
            }
            let v = raw::payload(self.datums[i]);
            let at = (v.as_ptr() as usize).checked_sub(base)?;
            if at < end || at + v.len() > limit {
                return None;
            }
            self.spans.push((at, v.len())); // alloc: Node
            end = at + v.len();
        }
        Some(end)
    }
}

/// D3288: whether this is code `c`'s first sight in the walk's `seen` (one bit per code, zeroed at the walk's start),
/// marking it seen.
#[inline(always)]
fn first_seen(seen: &mut [u64], c: u32) -> bool {
    let (w, b) = (c as usize / 64, 1u64 << (c % 64));
    let fresh = seen[w] & b == 0;
    seen[w] |= b;
    fresh
}

/// A build's scratch arena on the allocator's road, charged to `o` (`OpArena::scratch_of`).
fn arena_of(o: BuildOwner) -> OpArena {
    OpArena::scratch_of(o.map(|o| o.0), o.map_or(memledger::Kind::Region, |o| o.1))
}

/// `bytes` just allocated for a statement table or a source's memo, on its builder `o` (none: no charge). A table built to
/// be resident is not charged here: the fp cache's holder takes it (`take_into_cache`).
fn table_charge(o: BuildOwner, bytes: u64) -> Option<memledger::Charge> {
    use memledger::{Owner, Release};
    // the release the charge states (read back only), as `ChargedVec` states it: a portal's source holds it to the portal's end
    let release = |o: Owner| match o {
        Owner::Portal { .. } => Release::Portal,
        Owner::Session(_) => Release::Session,
        Owner::Cache(_) => Release::Evict,
        Owner::Statement { .. } | Owner::WorkerSet(_) | Owner::Op { .. } => Release::Statement,
    };
    o.map(|(o, k)| memledger::Charge::open(o, release(o), k, memledger::charge::Door::Heap, bytes))
}

/// CACHE-CHARGE: `c` (a cache scope's charge, `CacheScope::finish`) taken over by `into`: one charge for an image laid
/// under several scopes.
fn adopt(into: &mut Option<memledger::Charge>, c: Option<memledger::Charge>) {
    match (into.as_mut(), c) {
        (Some(a), Some(c)) => {
            a.absorb(c);
        }
        (None, c) => *into = c,
        (Some(_), None) => {}
    }
}

/// Who a table buffer's bytes are charged to from its take: its builder (a statement table: `table_charge`; none: no
/// charge), or the fp tables for an image built to be resident (taken under their cache scope, `take_held`).
#[derive(Clone, Copy)]
enum Held {
    By(BuildOwner),
    Resident,
}

/// A table buffer (`reuse::take`) and the charge its bytes are on: the charge declared first, so it ends before the
/// buffer is freed.
struct HeldBuf {
    held: Option<memledger::Charge>,
    buf: BuildBuf,
}

/// `reuse::take(words)` charged as `h` says. A resident image's buffer is taken under the fp tables' cache scope: a
/// fresh one is theirs as it is taken, a build-pool one's charge moves to theirs at the take (D4536).
fn take_held(words: usize, h: Held) -> HeldBuf {
    match h {
        Held::By(o) => {
            let buf = reuse::take(words); // alloc: Exec charged o
            // pagepool P3: a pool buffer's own charge on the builder (none: the calling thread's owner, as door 1's bytes)
            if buf.is_pooled() {
                buf.own(o.map_or_else(|| memledger::charge::context().0, |(o, _)| o));
            }
            HeldBuf { held: table_charge(o, buf.heap_bytes()), buf }
        }
        Held::Resident => {
            let mut scope = memledger::arena::cache_scope_for(FP_OWNER, memledger::Kind::Region);
            let buf = reuse::take_for(words, Some(&mut scope)); // alloc: Cold charged FP_OWNER
            buf.own(FP_OWNER);
            HeldBuf { held: scope.finish(), buf }
        }
    }
}

/// `c` at `to` bytes: what the allocations it covers hold now (grown, or one freed next).
fn charge_to(c: &mut Option<memledger::Charge>, to: u64) {
    if let Some(c) = c {
        let have = c.committed();
        if to > have {
            c.commit(to - have);
        } else {
            c.free(have - to);
        }
    }
}

/// A build's working memory refused by the allocator: the statement's out of memory (53200), never an abort.
#[cold]
fn refused(r: Refused) -> TableBuildError {
    let detail = format!("Failed on request of size {} in a Gravity dictionary build.", r.bytes); // alloc: Cold
    TableBuildError::Pg(Box::new(types_error::PgError::error("out of memory".to_string()).with_sqlstate(types_error::ERRCODE_OUT_OF_MEMORY).with_detail(detail))) // alloc: Cold
}

/// A stop inside a build's polled copy: the statement's cancel, or a refused buffer.
enum Stop {
    Canceled,
    Refused(Refused),
}

impl From<cancelpoll::Canceled> for Stop {
    fn from(_: cancelpoll::Canceled) -> Stop {
        Stop::Canceled
    }
}

impl From<Stop> for TableBuildError {
    fn from(s: Stop) -> TableBuildError {
        match s {
            Stop::Canceled => TableBuildError::Read(ReadError::Canceled),
            Stop::Refused(r) => refused(r),
        }
    }
}

/// A build's growing buffer on the allocator's road, charged to the build's owner (`OpArena::scratch_of`; none: charged to
/// no owner, as an allocator `Vec`'s bytes). It is alone in its arena: a growth carves the larger buffer in a fresh arena,
/// moves the elements `PIECE` bytes a tick and drops the old arena, so the old buffer is freed as
/// `cancelpoll::Step::reserve` frees it (a buffer sharing a bump arena keeps every old copy to the arena's drop).
struct Grow<T: ForgetSafe + Copy> {
    a: OpArena,
    v: AVec<T>,
    owner: BuildOwner,
}

impl<T: ForgetSafe + Copy> Grow<T> {
    fn new(owner: BuildOwner) -> Grow<T> {
        Grow { a: arena_of(owner), v: AVec::default(), owner }
    }

    fn len(&self) -> usize {
        self.v.len()
    }

    fn capacity(&self) -> usize {
        self.v.capacity()
    }

    /// The buffer's bytes (`cap_bytes` of the `Vec` it replaces).
    fn cap_bytes(&self) -> u64 {
        (self.v.capacity() * core::mem::size_of::<T>()) as u64
    }

    fn as_slice(&self) -> &[T] {
        self.v.as_slice(&self.a)
    }

    /// Room for `more` elements past the length: `Step::reserve`'s growth (the larger of what is asked and twice the
    /// capacity), the old buffer freed once its elements moved; nothing moves while the capacity holds.
    fn reserve(&mut self, step: &mut cancelpoll::Step, more: usize) -> Result<(), Stop> {
        if self.v.capacity() - self.v.len() >= more {
            return Ok(());
        }
        let a = arena_of(self.owner);
        let mut v = AVec::with_capacity(&a, (self.v.len() + more).max(self.v.capacity().saturating_mul(2))).map_err(Stop::Refused)?;
        let old = self.v.as_slice(&self.a);
        step.pieces(old.len(), (cancelpoll::PIECE / core::mem::size_of::<T>().max(1)).max(1), |r| v.extend_from_slice(&a, &old[r]).map_err(Stop::Refused))?;
        (self.a, self.v) = (a, v);
        Ok(())
    }

    /// `x` at the end, inside the capacity a `reserve` made.
    #[inline]
    fn push(&mut self, x: T) -> Result<(), Stop> {
        self.v.push(&self.a, x).map_err(Stop::Refused)
    }

    #[inline]
    fn extend_from_slice(&mut self, s: &[T]) -> Result<(), Stop> {
        self.v.extend_from_slice(&self.a, s).map_err(Stop::Refused)
    }

    /// `ScratchGate::cover_growth` for this buffer growing by `more` elements beside `live` bytes.
    fn cover_growth(&self, gate: &mut ScratchGate, more: usize, live: u64) {
        if self.v.capacity() - self.v.len() >= more {
            return;
        }
        let grown = (self.v.len() + more).max(self.v.capacity().saturating_mul(2)) * core::mem::size_of::<T>();
        gate.cover(live.saturating_add(grown as u64));
    }
}

impl Default for TextScratch {
    fn default() -> TextScratch {
        TextScratch::new()
    }
}

/// The distinct values of a part's rows in code order over a temporary store (the ranks build's `Dict`).
struct Store<'a> {
    bytes: &'a [u8],
    ents: &'a [(u32, u32)],
    /// D3887: the walk stopped for the statement's cancel (the table it fed is dropped).
    stopped: core::cell::Cell<bool>,
}

impl gravity_dict::Dict for Store<'_> {
    fn ncodes(&self) -> u32 {
        self.ents.len() as u32
    }

    fn entries(&self, f: &mut dyn FnMut(u32, gravity_dict::View)) {
        // D3887 (C9): an entry is one fingerprint (tens of ns of SipHash): a `batch` loop; a verbatim part's are seconds
        let mut step = cancelpoll::step("text_hosts: a temporary store's entries walked");
        for (c, &(off, len)) in self.ents.iter().enumerate() {
            if step.batch(c).is_err() {
                self.stopped.set(true);
                return;
            }
            f(c as u32, gravity_dict::View::of(&self.bytes[off as usize..(off + len) as usize]));
        }
    }
}

/// A table's buffer goes back to the build pool (`reuse`) when the table goes: the next build of a like size takes it.
impl Drop for FpTable {
    fn drop(&mut self) {
        drop(self.cached.get_mut().unwrap_or_else(|e| e.into_inner()).take());
        // pppress (B): a build's buffer again (the build pool keeps it, or the pool takes it back)
        self.buf.tag_cached(false);
        reuse::give(core::mem::take(&mut self.buf));
    }
}

/// G1-A: the resident fp tables' account in the memory ledger: a table's bytes move there when the cache takes it and back
/// at its drop (the last `Arc`), so a table evicted while a statement still holds it stays on the account while resident.
const FP_OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::FpTables));

/// R1b (D405 follow-on): the buffers every table build takes (`build_owned_bytes`, and the part-resident builds, D4278),
/// reused across builds and statements from one bounded process pool instead of a fresh zeroed buffer per build. Every
/// table's drop gives its buffer here, so every build takes from here too: a resident build that allocated fresh left
/// its dropped buffer kept for good on each rebuild (D4278: a statement repeated in one session kept one more set of
/// its resident tables' buffers a run, up to the cap).
/// A statement that declares a
/// derived table builds one per part (hundreds per statement over a large table), and those fresh buffers' first touch
/// dominated the statement's page faults — under memory pressure each of them a direct reclaim or compaction stall.
/// A reused buffer keeps its old words, never uninitialised memory: every region allocation is written whole by its
/// builder or zeroed by it (`Region::alloc_zeroed`, `Region::copy`, and like.rs / regexp.rs's `alloc`, each filled at
/// once), so a build reads no word it did not write, as over a zeroed buffer. The pool keeps at most `pool_bytes()`; a
/// buffer given past that frees the least recently given ones, and one under `MIN_WORDS` is not kept.
pub mod reuse {
    use core::sync::atomic::{AtomicU64, Ordering};
    use pgsync::Mutex;

    /// The pool's cap, a share of the machine and the grant pool (`cost::build_pool_cap`; explicit grants; H25: no fixed
    /// floor). A flat 768 MiB cap missed about 40 % of the takes of a statement that builds one table per
    /// part (the pool pinned at its cap: each miss a fresh build and its zero-fill faults); the 768 MiB floor that replaced
    /// it kept that much past a smaller grant pool or a small machine's RAM share. The idle release returns what the pool
    /// keeps.
    pub fn pool_bytes() -> usize {
        cap_of(ram_bytes(), gravity_driver::POOL.capacity())
    }

    /// `pool_bytes` over RAM `ram` and grant pool capacity `pool` (0 = unknown / unarmed).
    pub fn cap_of(ram: u64, pool: u64) -> usize {
        usize::try_from(gravity_program::cost::build_pool_cap(ram, pool)).unwrap_or(usize::MAX)
    }

    /// A process-resident store's cap `cap` (a cost:: share of the one process-cache budget: `cost::condition_cache_cap`,
    /// `cost::zone_store_cap`) over this machine and the grant pool as armed now.
    pub fn store_bytes(cap: fn(u64, u64) -> u64) -> usize {
        usize::try_from(cap(ram_bytes(), gravity_driver::POOL.capacity())).unwrap_or(usize::MAX)
    }

    /// The resident tables' cap over this machine and the grant pool as armed now (`cost::resident_tables_cap`).
    pub fn resident_tables_cap() -> u64 {
        gravity_program::cost::resident_tables_cap(ram_bytes(), gravity_driver::POOL.capacity())
    }
    use gravity_program::cost::DICT_POOL_MIN_WORDS as MIN_WORDS;
    /// A kept buffer serves a build of at least 1 / `FIT` of its size (no small table pins a large buffer).
    const FIT: usize = 4;

    /// A kept buffer and its bytes' charge (C8 move 7: `Cache(BuildPool)`, door 1), opened at its `give` from the giving
    /// thread's account; the idle release's given-back pages are this buffer's own (`uncommit` + `shrink`), and the drop as it
    /// leaves the pool (`leave`) moves its bytes, given-back pages included, to the taking or freeing thread's account.
    struct Kept {
        held: memledger::Charge,
        buf: super::BuildBuf,
    }

    struct Pool {
        /// D4537 7: `bufs`' own block (a `Kept` per buffer; each buffer has its own charge), refit after each change.
        charge: memledger::Charge,
        /// Given order, oldest first.
        bufs: Vec<Kept>,
        words: usize,
        /// Broker (b): the ghosts of the last [`GHOSTS`] buffers freed (past the cap or by the reclaimer), `(words, the
        /// pool's byte clock then)` (words 0: none or taken), in a ring at `freed_at`.
        freed: [(usize, u64); GHOSTS],
        freed_at: usize,
    }

    const GHOSTS: usize = 64;

    static POOL: Mutex<Pool> = Mutex::new(Pool { bufs: Vec::new(), words: 0, freed: [(0, 0); GHOSTS], freed_at: 0, charge: memledger::Charge::NONE }); // alloc: Cold

    /// G1-A: the build pool's owner in the memory ledger (its account `Acct::of(Consumer::BuildPool)`): each kept buffer's
    /// charge (`Kept`).
    const OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::BuildPool));

    /// A kept buffer leaves the pool for the calling thread (a take, or a free just after): its charge's drop moves its
    /// bytes, its given-back pages included, to the calling thread's account.
    fn leave(k: Kept) -> super::BuildBuf {
        let Kept { buf, held } = k;
        drop(held);
        buf
    }

    fn refit(p: &mut Pool) {
        if p.charge.owner() != OWNER {
            p.charge = memledger::Charge::open(OWNER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, 0);
        }
        p.charge.fit_committed((p.bufs.capacity() * core::mem::size_of::<Kept>()) as u64);
    }

    /// A buffer of at least `words` words: the smallest kept one that fits, else a fresh zeroed one.
    pub fn take(words: usize) -> super::BuildBuf {
        take_for(words, None)
    }

    /// `take` for a build under a cache scope: a kept buffer's charge goes to the scope's (`CacheScope::absorb`), not to
    /// this thread's account, where it read as unattributed until the cache's insert (D4536).
    /// pagepool P3: a pool buffer (kept or fresh) comes with its own charge, on the build pool or the calling thread's owner:
    /// the caller moves it to its holder (`BuildBuf::own`).
    pub fn take_for(words: usize, scope: Option<&mut memledger::arena::CacheScope>) -> super::BuildBuf {
        touch();
        if words >= MIN_WORDS {
            let mut p = POOL.lock().unwrap_or_else(|e| e.into_inner());
            let fits = |n: usize| n >= words && n / FIT <= words;
            let best = p.bufs.iter().enumerate().filter(|(_, k)| fits(k.buf.len())).min_by_key(|(_, k)| k.buf.len()).map(|(i, _)| i);
            if let Some(i) = best {
                // broker (b)'s loss signal: a take only the oldest tenth of the kept words could serve (a pool a tenth
                // smaller, which frees the oldest first, would have missed it)
                let tail = memledger::value::ext() && {
                    let (tenth, mut at) = (p.words / 10, 0);
                    // the first buffer starting past the tenth (its start: the words kept before it)
                    let past = p
                        .bufs
                        .iter()
                        .position(|k| {
                            let start = at;
                            at += k.buf.len();
                            start >= tenth
                        })
                        .unwrap_or(p.bufs.len());
                    !p.bufs[past..].iter().any(|k| fits(k.buf.len()))
                };
                let mut k = p.bufs.remove(i);
                p.words -= k.buf.len();
                refit(&mut p);
                k.buf.take_dead();
                let b = match scope {
                    Some(s) => {
                        let Kept { buf, held } = k;
                        s.absorb(held);
                        buf
                    }
                    None => leave(k),
                };
                drop(p);
                // pagepool P3: a pool buffer's own charge leaves the pool for the calling thread's owner, as door 1's bytes do
                if b.is_pooled() {
                    b.own(memledger::charge::context().0);
                }
                memledger::value::hit(memledger::Consumer::BuildPool);
                if tail {
                    memledger::value::tail_hit(memledger::Consumer::BuildPool);
                }
                return b;
            }
            if memledger::value::ext() {
                // broker (b)'s gain signal: the latest freed buffer that would have served this take
                if let Some(g) = p.freed.iter_mut().filter(|g| g.0 != 0 && fits(g.0)).max_by_key(|g| g.1) {
                    let at = g.1;
                    g.0 = 0;
                    memledger::value::ghost(memledger::Consumer::BuildPool, at, 1);
                }
            }
            drop(p);
            // a build the pool could have served had it kept a buffer: its miss, the fresh bytes the build faults in (no
            // price in time: its pages fault in the build's own loop)
            memledger::value::missed(memledger::Consumer::BuildPool, words as u64 * 8);
        }
        // pagepool P3: the page pool's (warm pages, no fresh fault) with the switch on
        super::BuildBuf::pool(words).unwrap_or_else(|| vec![0u64; words].into()) // alloc: Exec
    }

    /// Broker (b): a kept buffer of `words` words freed (past the cap or by the reclaimer): the pool's byte clock moves and
    /// the ring keeps its ghost (switched on by `memledger::value::ext`).
    fn ghost_mark(p: &mut Pool, words: usize) {
        if memledger::value::ext() {
            let at = memledger::value::evicted(memledger::Consumer::BuildPool, words as u64 * 8);
            let i = p.freed_at % GHOSTS;
            p.freed[i] = (words, at);
            p.freed_at = p.freed_at.wrapping_add(1);
        }
    }

    /// Broker (b): publish the pool's tail bytes (its oldest tenth of kept words; a take only they could serve is a tail
    /// hit, `take`, read live: the pool's buffers turn over faster than the broker's interval).
    pub fn publish_tail() {
        // on the memory watchdog's thread: no wait (busy, the last mark stays)
        let Some(p) = memledger::value::try_lock(&POOL) else { return };
        let words = p.words;
        drop(p);
        memledger::value::set_tail_bytes(memledger::Consumer::BuildPool, words as u64 * 8 / 10);
    }

    /// `buf` kept for a later `take` (its words as they are), the oldest kept ones freed past the cap.
    pub fn give(buf: impl Into<super::BuildBuf>) {
        let buf = buf.into();
        if buf.len() < MIN_WORDS || buf.len() * 8 > pool_bytes() {
            return;
        }
        let mut freed = Vec::new(); // alloc: Exec
        {
            let mut p = POOL.lock().unwrap_or_else(|e| e.into_inner());
            p.words += buf.len();
            // pagepool P3: a pool buffer's own charge moves to the pool's owner (no door-1 bytes to open a charge for)
            let held = if buf.is_pooled() {
                buf.own(OWNER);
                memledger::Charge::NONE
            } else {
                memledger::Charge::open(OWNER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, buf.len() as u64 * 8)
            };
            p.bufs.push(Kept { buf, held }); // alloc: Cold charged OWNER (Pool::charge, refit below)
            let cap = pool_bytes();
            // bounded("other: frees the oldest kept buffers until the pool is under its cap (build_pool_cap); a kept buffer is at least DICT_POOL_MIN_WORDS words, so the cap over that bounds the passes")
            while p.words * 8 > cap {
                let old = p.bufs.remove(0);
                let w = old.buf.len();
                p.words -= w;
                ghost_mark(&mut p, w);
                freed.push(leave(old)); // alloc: Exec
            }
            refit(&mut p);
        }
        drop(freed);
        touch();
    }

    /// No take or give for this long: every kept buffer's pages released (`MADV_DONTNEED`), the buffers still kept (a
    /// later take re-faults them, as a fresh one would). The threshold is the memory policy's (`mempolicy::keep`).
    pub use mempolicy::keep::IDLE_RELEASE_MS;
    static RAM: AtomicU64 = AtomicU64::new(u64::MAX);
    static STARTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

    /// The memory this server may use (read once; 0 when unknown): D3793, the published memory limit (a container's cgroup
    /// memory.max, `gravity_driver::machine::ram_reading`, D3314), else physical memory (`sysconf`). The auto caps of the part
    /// cache, the build pool, the zone store, the condition cache and the resident tables are shares of it: from physical
    /// memory, a 16 GiB container on a 31 GiB box sized each for 31 GiB.
    fn ram_bytes() -> u64 {
        let r = RAM.load(Ordering::Relaxed);
        if r != u64::MAX {
            return r;
        }
        #[cfg(target_os = "linux")]
        let r = gravity_driver::machine::ram_reading().unwrap_or_else(super::raw::phys_bytes);
        // D3175: the host-declared memory (gravity_driver::machine::ram_reading)
        #[cfg(target_family = "wasm")]
        let r = gravity_driver::machine::ram_reading().unwrap_or(0);
        #[cfg(not(any(target_os = "linux", target_family = "wasm")))]
        let r = 0;
        RAM.store(r, Ordering::Relaxed);
        r
    }
    static IDLE_MS: AtomicU64 = AtomicU64::new(IDLE_RELEASE_MS);
    static LAST_MS: AtomicU64 = AtomicU64::new(0);
    /// Idle releases done (tests, the trace).
    pub static IDLE_RELEASES: AtomicU64 = AtomicU64::new(0);
    /// D3063 residual: pressure releases done past the memory governor's bar (tests, the trace).
    pub static PRESSURE_RELEASES: AtomicU64 = AtomicU64::new(0);

    fn now_ms() -> u64 {
        pg_clock::mono_ms().max(0) as u64 + 1
    }

    /// A take or give: the idle clock restarts; the idle release's thread starts on the first one.
    fn touch() {
        LAST_MS.store(now_ms(), Ordering::Relaxed);
        start_reclaimer();
    }

    /// The idle release's thread started if it has not (the first take or give, or memory step 5b's reclaimer while it
    /// applies: the same thread ticks it). Starting it leaves the idle clock alone.
    pub fn start_reclaimer() {
        if !STARTED.load(Ordering::Relaxed) && !STARTED.swap(true, Ordering::AcqRel) {
            let _ = pgsync::thread::Builder::new().name("gravity-reuse-idle".into()).spawn(pagegive::pinned(idle_release)); // alloc: Cold
        }
    }

    /// D3003: a statement started (`residency::arm_part_cache`): the idle clock restarts, so the idle release (which also
    /// hands back the parked regions) runs only when no statement has started for `IDLE_RELEASE_MS`.
    pub fn note_statement() {
        touch();
    }

    /// Tests: the idle threshold.
    pub fn set_idle_ms(ms: u64) {
        IDLE_MS.store(ms.max(1), Ordering::Relaxed);
    }

    /// D3063 residual: what the process caches keep for no running statement, given back: the part cache to its trim target,
    /// the parked regions and slot sets, the allocator's retained memory. The idle release's, and the pressure release's
    /// past the governor's bar: a server stuck past the bar (every statement refused at once, so none reached the part
    /// cache's statement-start trim) kept 30 GB resident on a 30 GiB box with nothing running.
    fn release_process_caches() {
        // the parked slot sets first: a parked set keeps its parts in use, so the part cache trims past them only after
        gravity_driver::mem::grant::idle_release();
        crate::ops::scan::pgrc::residency::trim_part_cache();
        // LADDER-DESIGN 6a: the freed images' pages back from every thread that held them, then the purge
        mcx::give_back_caches();
    }

    /// The idle loop's mechanisms (memruntime's `loops::idle_loop` runs the loop on this thread and asks the memory policy
    /// when each step is due): memory step 5b's reclaimer while it applies, the release past the governor's bar, and the
    /// idle release.
    static IDLE_HOOKS: memruntime::loops::IdleHooks = memruntime::loops::IdleHooks {
        reclaim_applies: gravity_driver::mem::reclaim::applies,
        reclaim_tick: reclaim_tick,
        reclaim_tick_ms: gravity_driver::mem::reclaim::TICK_MS,
        idle_ms,
        last_ms,
        now_ms,
        release_caches: release_process_caches,
        release_kept,
        pressure_released,
        idle_released,
    };

    fn idle_release() {
        memruntime::loops::idle_loop(&IDLE_HOOKS)
    }

    fn reclaim_tick() {
        gravity_driver::mem::reclaim::tick();
    }

    fn idle_ms() -> u64 {
        IDLE_MS.load(Ordering::Relaxed)
    }

    fn last_ms() -> u64 {
        LAST_MS.load(Ordering::Relaxed)
    }

    /// Every kept buffer's pages given back; the pages leave the pool's account (G1-A: each buffer's at most once, a second
    /// idle release finds them given already): off its charge (`uncommit`) with no room kept for them (`shrink`), so the
    /// pool lines net to zero.
    fn release_kept() {
        let mut p = POOL.lock().unwrap_or_else(|e| e.into_inner());
        // bounded("other: one page release per kept buffer; the pool's cap (build_pool_cap) over DICT_POOL_MIN_WORDS words bounds the buffers")
        for k in p.bufs.iter_mut() {
            // pagepool P3: a pool buffer's whole runs go dead (the purger releases them), off its own charge
            if k.buf.give_dead() {
                continue;
            }
            let bytes = k.buf.len() as u64 * 8;
            let released = super::raw::release(k.buf.as_mut_ptr() as *mut u8, k.buf.len() * 8);
            let fresh = released.saturating_sub(bytes - k.held.committed());
            k.held.uncommit(fresh);
            k.held.shrink(fresh);
        }
    }

    fn pressure_released() {
        PRESSURE_RELEASES.fetch_add(1, Ordering::Relaxed);
        if gravity_driver::env::set("PGRUST_GRAVITY_REPORT") {
            eprintln!("LOG:  gravity: pressure release past the memory governor's bar: {}", memory_census()); // alloc: Cold
        }
    }

    /// D3003 (c): a process idle this long also hands the parked regions and slot sets back (a backend's own idle release
    /// never runs when each session ends right after its statement).
    fn idle_released() {
        IDLE_RELEASES.fetch_add(1, Ordering::Relaxed);
        if gravity_driver::env::set("PGRUST_GRAVITY_REPORT") {
            // the server log's own line (no backend on this thread): what an idle process still holds, by holder
            eprintln!("LOG:  gravity: idle memory census: {}", memory_census()); // alloc: Cold
        }
    }

    /// D3063 residual: the process's resident bytes (the governor's projection) and what each process-lived holder keeps, in MiB: the
    /// grant pool (held / capacity) and its parked regions and slot sets, the build-buffer pool (kept; its pages are
    /// released at idle), the resident fp tables, pgrc's part cache, the zone store and the memory contexts' accounted
    /// bytes. Read under PGRUST_GRAVITY_REPORT at each idle release, so a long-lived server's residue has an owner.
    pub fn memory_census() -> String {
        let mib = |b: u64| b >> 20;
        let pool = &gravity_driver::POOL;
        // the memory watchdog's last resident sample (RssAnon, every 100 ms) moved by the contexts' growth since
        let rss = mcx::governor::projected() as u64;
        let (_, _, fp_n, fp_bytes, _) = super::fp_tables_resident();
        // D3243 diagnostic: the allocator's split of the same resident bytes, while its live blocks are counted
        let alloc = mcx::governor::allocator_census(rss as usize).map_or_else(String::new, |c| format!(" {c}")); // alloc: Cold
        // D3872: the residency passes run, the last one's and the longest one's time, and the bytes the last one read
        let residency = {
            use core::sync::atomic::Ordering::Relaxed;
            use gravity_driver::mem::resident::{PASSES, PASS_NS_LAST, PASS_NS_MAX, PASS_SCANNED_LAST};
            format!(" residency passes={} last_us={} max_us={} scanned_mib={}", PASSES.load(Relaxed), PASS_NS_LAST.load(Relaxed) / 1000, PASS_NS_MAX.load(Relaxed) / 1000, PASS_SCANNED_LAST.load(Relaxed) >> 20) // alloc: Cold
        };
        // memory step 5: Gravity's one memory budget and its consumers, unless the broker is off
        let budget = gravity_driver::mem::budget::snapshot().map_or_else(String::new, |b| format!(" memory budget: {} {}", b.line(), gravity_driver::mem::reclaim::line())); // alloc: Cold
        format!( // alloc: Cold
            "rss_anon={} pool_held={}/{} regions={} slots={} build_pool={} fp_tables={}/{} part_cache={}/{} zone_store={} mcx={} part_cache_held={}{alloc} (MiB){}{residency}",
            mib(rss),
            mib(pool.held()),
            mib(pool.capacity()),
            mib(gravity_driver::REGIONS.stats().bytes as u64),
            mib(gravity_driver::DEPOT.stats().bytes as u64),
            mib(held().1 as u64 * 8),
            mib(fp_bytes),
            fp_n,
            mib(pgrc_read::partcache::bytes()),
            mib(pgrc_read::partcache::budget()),
            mib(crate::ops::scan::pgrc::granule_source::zone_store_held() as u64),
            mib(mcx::global_footprint::bytes() as u64),
            // after mcx=: the e2e census readers match `part_cache=<held>/<budget> zone_store=`
            mib(crate::ops::scan::pgrc::granule_source::part_cache_held()),
            budget,
        )
    }

    /// The pool's kept buffers and words (tests).
    pub fn held() -> (usize, usize) {
        // D4398: on the memory ladder's pass, nothing read while another thread holds the pool (the pass leaves it out)
        let Some(p) = memledger::ladder_pass::lock_or_skip(&POOL) else { return (0, 0) };
        (p.bufs.len(), p.words)
    }

    /// Memory step 5: the build pool's reclaim for the memory budget (5b's reclaimer): the oldest kept
    /// buffers freed until the pool keeps at most `bytes`. The bytes freed.
    pub fn release_to(bytes: u64) -> u64 {
        let mut freed = Vec::new(); // alloc: Exec
        let mut n = 0u64;
        {
            // D4398: on the memory ladder's pass, nothing freed while another thread holds the pool
            let Some(mut p) = memledger::ladder_pass::lock_or_skip(&POOL) else { return 0 };
            // bounded("other: the memory budget's reclaim frees the oldest kept buffers down to bytes, one a pass; the pool's cap over DICT_POOL_MIN_WORDS words bounds them")
            while p.words as u64 * 8 > bytes && !p.bufs.is_empty() {
                let old = p.bufs.remove(0);
                let w = old.buf.len();
                p.words -= w;
                n += w as u64 * 8;
                ghost_mark(&mut p, w);
                freed.push(leave(old)); // alloc: Exec
            }
            refit(&mut p);
        }
        drop(freed);
        n
    }
}

impl FpTable {
    pub fn header(&self) -> *const gravity_dict::table::Header {
        self.hdr
    }

    /// The resident cache took the table: what it allocates (`alloc_bytes`; the cache's census counts `bytes`) on the fp
    /// tables' charge from now on: the one its build's cache scopes took (`take_held`, `adopt`) kept, else a building
    /// owner's (`table_charge`) ended first, so the bytes are on one owner at a time. D4529: fitted to `bytes`, the
    /// capacity past the lists' lengths went back to the building thread's account and stayed there, unattributed, for
    /// the table's life.
    /// PINS: a pin on the table for the calling thread's portal or statement, when the cache holds its bytes (the fp tables'
    /// charge); `None` otherwise.
    fn pin(&self) -> Option<memledger::pins::PinGuard> {
        let cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if !cached.as_ref().is_some_and(|c| c.owner() == FP_OWNER) {
            return None;
        }
        let bytes = self.bytes();
        drop(cached);
        let set = self.pins.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(|| memledger::pins::PinSet::new(bytes)).clone();
        Some(set.pin())
    }

    /// PINS: the cache evicted the table: its holders' pins move into evicted until the last lets go.
    fn evicted(&self) {
        if let Some(p) = self.pins.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            p.evict();
        }
    }

    fn take_into_cache(&self) {
        let bytes = self.alloc_bytes();
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        match cached.as_mut() {
            // CACHE-CHARGE: the fp tables' charge its build's cache scopes took, fitted to the image's bytes (a build-pool
            // buffer the scope did not take comes off this thread, the building one)
            Some(c) if c.owner() == FP_OWNER => c.fit_committed(bytes),
            _ => {
                drop(cached.take());
                *cached = Some(memledger::Charge::open(FP_OWNER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, bytes));
            }
        }
        // pagepool P3: a pool buffer's own charge with it; pppress (B): and its pool tag, the cache's, so the present
        // sweep's cache term holds the resident fp tables as the census does
        self.buf.own(FP_OWNER);
        self.buf.tag_cached(true);
    }

    pub fn table(&self) -> gravity_dict::Fp128 {
        debug_assert!(!self.hdr.is_null(), "an entry-runs table has no Fp128 header");
        raw::fp128_at(self.hdr)
    }

    /// H08 (road code.sparse's table form): a condition table (EQ, LIKE, a byte range) as its `Bits` table; `None` for a
    /// table of another element.
    pub fn bits(&self) -> Option<gravity_dict::table::Bits> {
        raw::bits_at(self.hdr)
    }

    /// The resident bytes (the buffer's written bytes; a verbatim part's ranks).
    pub fn bytes(&self) -> u64 {
        self.used + self.ranks.as_ref().map_or(0, Ranks::bytes)
    }

    /// D4529: what the table allocates, the charge's bytes: its buffer, its ranks' lists and its render offsets at their
    /// allocated sizes.
    fn alloc_bytes(&self) -> u64 {
        self.buf.heap_bytes() + self.ranks.as_ref().map_or(0, Ranks::alloc_bytes) + self.off.as_ref().map_or(0, |o| o.len() as u64 * 4)
    }

    /// A verbatim part's codes; `None` for a dictionary-coded part.
    pub fn ranks(&self) -> Option<&Ranks> {
        self.ranks.as_ref()
    }

    /// An entry-runs table's run records (two words each, `[lo, hi]`) and bucket offsets (granule_source/entry_runs.rs); `None` for any other.
    pub fn runs(&self) -> Option<(&[u64], &[u32])> {
        Some((&self.buf[..], self.off.as_deref().filter(|_| !self.renders)?))
    }

    /// A render table's image of `code` (`render_table`): its 4B-U varlena's address and byte length; `None` for a code
    /// with no live row, or a table of another kind.
    #[inline]
    pub fn render(&self, code: u32) -> Option<(u64, u32)> {
        if !self.renders {
            return None;
        }
        let at = *self.off.as_deref()?.get(code as usize)?;
        let w = self.buf.get(at as usize)?;
        Some((w as *const u64 as u64, ((*w as u32) >> 2) - 4))
    }

    /// A verbatim part's renders: the images in `buf`, each code's word offset in `off` (`render_table`).
    fn of_renders(buf: Vec<u64>, off: Box<[u32]>) -> FpTable {
        FpTable { cached: Mutex::new(None), used: (buf.len() * 8 + off.len() * 4) as u64, buf: buf.into(), hdr: core::ptr::null(), ranks: None, off: Some(off), renders: true, pins: Mutex::new(None) }
    }

    /// The table of entry runs `buf` / `off` (granule_source/entry_runs.rs `build`): resident like the part's `Fp128` table.
    pub(crate) fn of_runs(buf: Vec<u64>, off: Box<[u32]>) -> FpTable {
        FpTable { cached: Mutex::new(None), used: (buf.len() * 8 + off.len() * 4) as u64, buf: buf.into(), hdr: core::ptr::null(), ranks: None, off: Some(off), renders: false, pins: Mutex::new(None) }
    }

    /// The table's `Fp128` entries, 16 B per code: `None` for a table of another element, one with faulted entries, or an
    /// entry-runs table.
    pub(crate) fn table_entries(&self) -> Option<&[u8]> {
        if self.hdr.is_null() {
            return None;
        }
        raw::fp128_entries(self)
    }
}

struct FpCache {
    /// D4537 7: the map's own bytes (its slots and the specs; each table has its own charge), refit after each change.
    charge: memledger::Charge,
    map: HashMap<([u8; 16], u32, u64), FpEntry>,
    bytes: u64,
    tick: u64,
    /// The entries' specs' heap bytes.
    specs: u64,
}

struct FpEntry {
    table: Arc<FpTable>,
    part: Weak<OpenPart>,
    used: u64,
    /// The spec a condition table was built for (`cond_table`): a hit needs the whole spec equal, not its fingerprint alone.
    spec: Option<TableSpec>,
    /// Memory step 6a: the statement epoch of the entry's latest counted use, and the broker seconds of its latest two
    /// statements' uses (latest low, the one before high; `FpEntry::note_reuse`).
    reuse_epoch: u64,
    reuse_secs: u64,
}

impl Drop for FpEntry {
    /// An entry leaves the cache (evicted, replaced, retired with its part, dropped with it): the scans that still
    /// reference its table hold it evicted.
    fn drop(&mut self) {
        self.table.evicted();
    }
}

impl FpEntry {
    fn spec_bytes(&self) -> u64 {
        self.spec.as_ref().map_or(0, |s| s.func.heap_bytes() as u64)
    }

    /// Memory step 6a: a hit while the broker runs. A statement takes each table once (`StmtTables`), so the first hit in
    /// a new statement epoch, 2 s or more after the latest, is a new statement's (the part counter's fallback rule).
    fn note_reuse(&mut self, now: u32) {
        let epoch = pgrc_read::partcache::statement_epoch();
        if epoch == self.reuse_epoch {
            return;
        }
        self.reuse_epoch = epoch;
        let last = self.reuse_secs as u32;
        if now.saturating_sub(last) >= 2 {
            self.reuse_secs = (u64::from(last) << 32) | u64::from(now);
        }
    }
}

impl FpCache {
    fn new() -> FpCache {
        let charge = memledger::Charge::open(FP_OWNER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, 0);
        FpCache { map: HashMap::new(), bytes: 0, tick: 0, specs: 0, charge }
    }

    fn refit(&mut self) {
        const SLOT: usize = core::mem::size_of::<([u8; 16], u32, u64)>() + core::mem::size_of::<FpEntry>() + 1;
        self.charge.fit_committed((self.map.capacity() * SLOT) as u64 + self.specs);
    }

    fn remove(&mut self, key: &([u8; 16], u32, u64)) {
        if let Some(e) = self.map.remove(key) {
            self.bytes -= e.table.bytes();
            self.specs -= e.spec_bytes();
            self.refit();
        }
    }

    fn sweep_dead(&mut self) {
        let FpCache { map, bytes, specs, .. } = &mut *self;
        let mut dead = 0u64;
        // retain's one pass over the map (no loop of ours, so no bounded marker): the entries whose part is gone, at most the
        // cache's entries (its byte cap bounds them), go with their bytes, a map removal each, as `remove` does
        map.retain(|_, e| {
            let gone = e.part.strong_count() == 0;
            if gone {
                *bytes -= e.table.bytes();
                *specs -= e.spec_bytes();
                dead += 1;
            }
            !gone
        });
        self.refit();
        FP_RETIRED.fetch_add(dead, Ordering::Relaxed);
    }

    /// Whether `key`'s entry was built for `part` itself (D1137: the uuid is `(dev, ino, len, footer_off)`, which a later part
    /// reusing a dead part's inode repeats; the entry's `Weak` keeps its part's allocation, so no later part shares the address).
    fn owned_by(&self, key: &([u8; 16], u32, u64), part: &Arc<OpenPart>) -> Option<bool> {
        self.map.get(key).map(|e| core::ptr::eq(e.part.as_ptr(), Arc::as_ptr(part)))
    }

    /// The table of `key` for `part` (under `spec` when given): an entry of another part, dead or alive, is dropped and misses.
    fn hit(&mut self, key: &([u8; 16], u32, u64), part: &Arc<OpenPart>, spec: Option<&TableSpec>) -> Option<Arc<FpTable>> {
        if !self.owned_by(key, part)? {
            self.remove(key);
            FP_RETIRED.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let e = self.map.get_mut(key).filter(|e| spec.is_none_or(|s| e.spec.as_ref() == Some(s)))?;
        if memledger::value::ext() {
            let tail = FP_TAIL_TICK.load(Ordering::Relaxed);
            if tail != 0 && e.used <= tail {
                memledger::value::tail_hit(memledger::Consumer::FpTables);
            }
        }
        self.tick += 1;
        e.used = self.tick;
        let now = pgrc_read::partcache::now_sec();
        if now != 0 {
            e.note_reuse(now);
        }
        memledger::value::hit(memledger::Consumer::FpTables);
        Some(Arc::clone(&e.table))
    }
}

/// The resident tables' cap in bytes: `pgrust.gravity_fp_cache_mb`, else the machine's share (`cost::resident_tables_cap`:
/// RAM / 8, not bounded by the grant pool).
pub fn fp_cache_cap() -> u64 {
    crate::inputs::custom_guc_u64(FP_CACHE_GUC).map_or_else(reuse::resident_tables_cap, |mb| mb << 20)
}

/// Memory step 5: the cap the last insert evicted to (u64::MAX: no insert yet), so the memory budget reads the cap on any
/// thread without a GUC store.
static FP_CAP_SEEN: AtomicU64 = AtomicU64::new(u64::MAX);

/// Memory step 5: the resident tables' cap as the memory budget reads it: the last insert's, else the
/// machine's share.
pub fn fp_cache_cap_seen() -> u64 {
    match FP_CAP_SEEN.load(Ordering::Relaxed) {
        u64::MAX => reuse::resident_tables_cap(),
        c => c,
    }
}

/// Memory step 5: the bytes of the resident tables some reader still holds (an `Arc` past the cache's own): the memory
/// budget's floor for the fp tables (dropping such an entry frees nothing until its reader lets go).
pub fn fp_pinned_bytes() -> u64 {
    // D4398: as fp_tables_resident
    let Some(g) = memledger::ladder_pass::lock_or_skip(&FP_CACHE) else { return 0 };
    g.as_ref().map_or(0, |c| c.map.values().filter(|e| Arc::strong_count(&e.table) > 1).map(|e| e.table.bytes()).sum())
}

/// Memory step 5: the fp tables' reclaim for the memory budget (5b's reclaimer): the least recently used entries no reader
/// holds dropped until the cache keeps at most `bytes`. The bytes dropped.
pub fn fp_cache_trim_to(bytes: u64) -> u64 {
    let mut dropped = Vec::new(); // alloc: Exec
    let mut n = 0u64;
    {
        // D4398: on the memory ladder's pass, nothing dropped while another thread holds the cache
        let Some(mut g) = memledger::ladder_pass::lock_or_skip(&FP_CACHE) else { return 0 };
        let Some(c) = g.as_mut() else { return 0 };
        if c.bytes <= bytes {
            return 0;
        }
        let mut lru: Vec<(u64, ([u8; 16], u32, u64))> = c.map.iter().filter(|(_, e)| Arc::strong_count(&e.table) == 1).map(|(k, e)| (e.used, *k)).collect(); // alloc: Exec
        // bounded("other: the memory budget's reclaim: the cache's unpinned entries (its byte cap bounds them) by last use, one sort of integer keys")
        lru.sort_unstable();
        // bounded("other: the memory budget's reclaim drops the least recently used unpinned entries down to bytes, at most the cache's entries")
        for (_, k) in lru {
            if c.bytes <= bytes {
                break;
            }
            if let Some(e) = c.map.remove(&k) {
                let b = e.table.bytes();
                c.bytes -= b;
                c.specs -= e.spec_bytes();
                n += b;
                fp_evicted(&k, b);
                dropped.push(e); // alloc: Exec
            }
        }
        c.refit();
    }
    drop(dropped);
    n
}

/// Memory step 6a: the bytes of the resident tables a statement before the latest used inside 60 / 300 / 900 s
/// (`pgrc_read::REUSE_WINDOWS`) at broker second `now`: the fp tables' re-used working set.
pub fn fp_reused_bytes(now: u32) -> [u64; 3] {
    let mut r = [0u64; 3];
    let g = FP_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(c) = g.as_ref() else { return r };
    // bounded("other: the broker's census on the memory watchdog's thread, outside every statement's kernel call; one pass over the cache's entries")
    for e in c.map.values() {
        let prev = (e.reuse_secs >> 32) as u32;
        if prev == 0 {
            continue;
        }
        // bounded("const-domain: REUSE_WINDOWS, the census's fixed windows")
        for (i, w) in pgrc_read::REUSE_WINDOWS.iter().enumerate() {
            if now.saturating_sub(prev) <= *w {
                r[i] += e.table.bytes();
            }
        }
    }
    r
}

/// The census: `(hits, builds, resident tables, their bytes, entries dropped with their part)`.
pub fn fp_tables_resident() -> (u64, u64, u64, u64, u64) {
    // D4398: on the memory ladder's pass, nothing read while another thread holds the cache (the pass leaves it out)
    let Some(g) = memledger::ladder_pass::lock_or_skip(&FP_CACHE) else { return (0, 0, 0, 0, 0) };
    let (n, b) = g.as_ref().map_or((0, 0), |c| (c.map.len() as u64, c.bytes));
    memledger::value::flush();
    (memledger::value::stats(memledger::Consumer::FpTables).hits, FP_BUILT.load(Ordering::Relaxed), n, b, FP_RETIRED.load(Ordering::Relaxed))
}

/// Drop every resident table (a rig's stand-in for a restart): the count dropped.
pub fn fp_cache_drop_all() -> u64 {
    let mut g = FP_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(c) = g.as_mut() else { return 0 };
    let n = c.map.len() as u64;
    c.map.clear();
    c.bytes = 0;
    c.specs = 0;
    c.refit();
    n
}

fn retire_fp(uuid: [u8; 16]) -> u64 {
    let mut g = FP_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(c) = g.as_mut() else { return 0 };
    let FpCache { map, bytes, specs, .. } = &mut *c;
    let mut n = 0u64;
    // retain's one pass over the map (no loop of ours, so no bounded marker): one retired part's tables, a few per column
    // (its fingerprint, bpchar, renders and condition tables), go with their bytes, as `remove` does
    map.retain(|k, e| {
        let keep = k.0 != uuid;
        if !keep {
            *bytes -= e.table.bytes();
            *specs -= e.spec_bytes();
            n += 1;
        }
        keep
    });
    c.refit();
    FP_RETIRED.fetch_add(n, Ordering::Relaxed);
    n
}

/// D3206: the execution's memo of the fingerprint cache's tables (a part's `Fp128` table, its entry runs, its renders, a
/// condition table), held by the execution's `Resolved` and gone with it: on a process-cache miss (the cache off, a cold
/// part, an evicted table) the first source that needs a table builds it under the key's own lock
/// and every other source of the statement waits for it and shares it, so a table is built once per statement, not once
/// per worker. Never across statements: `pgrust.gravity_fp_cache_mb = 0` still keeps nothing for the next one (the
/// fingerprint cache ruling). Each table it built is held to the statement's end within `cap` bytes (the grant, LRU like
/// the statement's shared tables); one past the cap is kept only while a source pins it.
pub struct FpMemo {
    map: Mutex<HashMap<([u8; 16], u32, u64), MemoEntry>>, // alloc: Exec
    bytes: AtomicU64,
    cap: AtomicU64,
    tick: AtomicU64,
    /// Below `pgrust.condition_cache = rows`, a condition table is the statement's own,
    /// never looked up in or kept in the process cache (`cond_table_in`).
    cross: core::sync::atomic::AtomicBool,
}

struct MemoEntry {
    slot: Arc<MemoSlot>,
    /// The strong hold within the cap (`None`: past it, or not built yet).
    held: Option<FpPin>,
    used: u64,
}

/// One build of a key. The first source claims it (`MEMO_BUILDING`) and builds with no lock held. A source that needs it
/// meanwhile spins a moment, then parks on its waiter, published in the slot (`waiter::park_timeout`); the builder
/// unparks every one at the publish, so all wake at once (a lock taken by each waiter in turn handed the table on one
/// wake-up at a time, 15 of them per table at W16). A slot whose table is gone (dropped past the cap), failed or is
/// another part's is replaced by a fresh one.
#[derive(Default)]
struct MemoSlot {
    state: AtomicU8,
    inner: Mutex<MemoInner>,
}

#[derive(Default)]
struct MemoInner {
    built: Option<MemoBuilt>,
    /// The parked sources' waiters, unparked at the publish.
    waiters: Vec<waiter::WakerHandle>, // alloc: Exec
}

const MEMO_EMPTY: u8 = 0;
const MEMO_BUILDING: u8 = 1;
const MEMO_DONE: u8 = 2;

/// The checks a waiting source spins before it parks, and its park's bound (the publish unparks it at once; the bound
/// only covers a wake the waiter lost).
const MEMO_SPINS: u32 = 1024;
const MEMO_PARK: core::time::Duration = core::time::Duration::from_millis(10);

struct MemoBuilt {
    /// Empty when the build failed (the failing source raised its error; a later source builds again).
    table: Weak<FpTable>,
    part: Weak<OpenPart>,
    spec: Option<TableSpec>,
}

impl MemoSlot {
    /// Publish the build (`None`: it failed or unwound) and wake every parked source.
    fn publish(&self, built: Option<MemoBuilt>) {
        pgsync::lock(&self.inner).built = built;
        self.state.store(MEMO_DONE, Ordering::Release);
        let waiters = core::mem::take(&mut pgsync::lock(&self.inner).waiters);
        // bounded("other: one unpark per source parked on this build, at most the statement's sources (its workers and the leader)")
        for w in waiters {
            let _ = waiter::unpark(w);
        }
    }

    /// Wait for the build another source claimed: `Some(true)` once it is published, `Some(false)` when it failed, `None`
    /// when this source's statement was canceled meanwhile (C9: a park a tick, its waiter taken back).
    fn wait_built(&self) -> Option<bool> {
        for _ in 0..MEMO_SPINS {
            match self.state.load(Ordering::Acquire) {
                MEMO_DONE => return Some(true),
                MEMO_EMPTY => return Some(false),
                _ => core::hint::spin_loop(),
            }
        }
        // the waiter is published before the last check (a publish after it unparks this source)
        let me = waiter::current_handle();
        pgsync::lock(&self.inner).waiters.push(me);
        let mut step = cancelpoll::step("text_hosts memo: a source's wait on another source's build");
        loop {
            match self.state.load(Ordering::Acquire) {
                MEMO_DONE => return Some(true),
                MEMO_EMPTY => return Some(false),
                _ => {
                    if step.tick().is_err() {
                        pgsync::lock(&self.inner).waiters.retain(|&w| w != me);
                        return None;
                    }
                    let _ = waiter::park_timeout(MEMO_PARK);
                }
            }
        }
    }
}

/// The claimed build's publish, run on the way out of the build whatever happens (an unwound build publishes none, so
/// no source waits on it forever).
struct Publish<'a> {
    slot: &'a MemoSlot,
    built: Option<MemoBuilt>,
}

impl Drop for Publish<'_> {
    fn drop(&mut self) {
        self.slot.publish(self.built.take());
    }
}

static MEMO_SHARED: AtomicU64 = AtomicU64::new(0);

/// D3206's census: the tables a statement's source took from another source's build or load (process-wide, monotonic).
pub fn fp_memo_shared() -> u64 {
    MEMO_SHARED.load(Ordering::Relaxed)
}

impl Default for FpMemo {
    fn default() -> FpMemo {
        FpMemo::new()
    }
}

impl FpMemo {
    pub fn new() -> FpMemo {
        FpMemo { map: Mutex::new(HashMap::new()), bytes: AtomicU64::new(0), cap: AtomicU64::new(0), tick: AtomicU64::new(0), cross: core::sync::atomic::AtomicBool::new(true) } // alloc: Exec
    }

    /// The execution's `pgrust.condition_cache`, set before any source binds (`Resolved::new`).
    pub fn set_cross_statement(&self, on: bool) {
        self.cross.store(on, Ordering::Relaxed);
    }

    /// Whether this execution may reuse and keep work across statements (`set_cross_statement`; on by default).
    pub fn cross_statement(&self) -> bool {
        self.cross.load(Ordering::Relaxed)
    }

    /// The bytes held to the statement's end (the statement's grant; 0: a table lives only while a source pins it).
    pub fn set_cap(&self, bytes: u64) {
        self.cap.store(bytes, Ordering::Relaxed);
    }

    /// The census: `(tables held to the statement's end, their bytes)`.
    pub fn held(&self) -> (u64, u64) {
        let m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        (m.values().filter(|e| e.held.is_some()).count() as u64, self.bytes.load(Ordering::Relaxed))
    }

    /// The statement's table at `key` of `part` (under `spec` for a condition table) that `fits`, else `make`'s, kept for
    /// the statement's other sources. The first source to claim the key's slot runs `make` (holding no lock); a source that
    /// needs the same key meanwhile waits for it (`make` waits on no other key). A `make` that fails leaves the key to the
    /// next source's build.
    fn get<E>(&self, key: ([u8; 16], u32, u64), part: &Arc<OpenPart>, spec: Option<&TableSpec>, fits: impl Fn(&FpTable) -> bool, make: impl FnOnce() -> Result<Arc<FpTable>, E>) -> Result<Arc<FpTable>, E> {
        let mut make = Some(make);
        // two rounds: a slot another source filled with a table that does not serve this one (gone, failed, another
        // part's) is replaced and built again; a second such round builds outside the memo
        for _ in 0..2 {
            let slot = {
                let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
                let tick = self.tick.fetch_add(1, Ordering::Relaxed);
                let e = m.entry(key).or_insert_with(|| MemoEntry { slot: Arc::new(MemoSlot::default()), held: None, used: tick }); // alloc: Exec
                e.used = tick;
                Arc::clone(&e.slot)
            };
            if slot.state.compare_exchange(MEMO_EMPTY, MEMO_BUILDING, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                let mut publish = Publish { slot: &slot, built: None };
                let made = (make.take().expect("one build per call"))();
                let table = made.as_ref().map_or_else(|_| Weak::new(), Arc::downgrade);
                publish.built = Some(MemoBuilt { table, part: Arc::downgrade(part), spec: spec.cloned() }); // alloc: Exec
                drop(publish);
                return match made {
                    Ok(t) => {
                        self.hold(&key, &t);
                        Ok(t)
                    }
                    Err(e) => {
                        self.renew(&key, &slot);
                        Err(e)
                    }
                };
            }
            match slot.wait_built() {
                Some(true) => {
                    let got = pgsync::lock(&slot.inner);
                    if let Some(b) = got.built.as_ref().filter(|b| core::ptr::eq(b.part.as_ptr(), Arc::as_ptr(part)) && b.spec.as_ref() == spec) {
                        if let Some(t) = b.table.upgrade().filter(|t| fits(t)) {
                            MEMO_SHARED.fetch_add(1, Ordering::Relaxed);
                            return Ok(t);
                        }
                    }
                }
                Some(false) => {}
                // canceled: this source builds outside the memo, where the build's own polls stop it
                None => break,
            }
            self.renew(&key, &slot);
        }
        (make.take().expect("not built"))()
    }

    /// A fresh slot at `key` in place of `slot` (another source may have renewed it already).
    fn renew(&self, key: &([u8; 16], u32, u64), slot: &Arc<MemoSlot>) {
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = m.get_mut(key).filter(|e| Arc::ptr_eq(&e.slot, slot)) {
            e.slot = Arc::new(MemoSlot::default()); // alloc: Exec
        }
    }

    /// Hold `t` at `key` to the statement's end, the least recently used holds dropped past the cap (their tables stay
    /// shared while a source pins them).
    fn hold(&self, key: &([u8; 16], u32, u64), t: &Arc<FpTable>) {
        let cap = self.cap.load(Ordering::Relaxed);
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let Some(e) = m.get_mut(key) else { return };
        if let Some(old) = e.held.replace(FpPin::of(t)) {
            self.bytes.fetch_sub(old.bytes(), Ordering::Relaxed);
        }
        self.bytes.fetch_add(t.bytes(), Ordering::Relaxed);
        // bounded("other: drops the oldest holds until the statement's memo is under its cap, one hold a pass; the memo holds a few tables per part the statement reads")
        while self.bytes.load(Ordering::Relaxed) > cap {
            let Some(oldest) = m.iter().filter(|(_, e)| e.held.is_some()).min_by_key(|(_, e)| e.used).map(|(k, _)| *k) else { break };
            let old = m.get_mut(&oldest).and_then(|e| e.held.take()).expect("held");
            self.bytes.fetch_sub(old.bytes(), Ordering::Relaxed);
        }
    }
}

/// `fp_table_as` through the statement's memo (`None`: this source's own lookup, as before D3206): the process cache's
/// table, else the statement's, else one source's build of it for every source.
pub fn fp_table_in(memo: Option<&FpMemo>, part: &Arc<OpenPart>, attno: u32, dict: &DictView<'_>, over: u8, bpchar: bool) -> Result<Arc<FpTable>, TableBuildError> {
    let Some(memo) = memo else { return fp_table_as(part, attno, dict, over, bpchar) };
    let key = (part.uuid(), attno, if bpchar { FP_BPCHAR } else { 0 });
    let ncodes = gravity_dict::Dict::ncodes(dict) as usize;
    if let Some(t) = cached(&key, part).filter(|t| t.table().ncodes() as usize == ncodes) {
        fires::fire_shared("dict.fp_resident"); // shapecheck fire signal (REGISTRY H24, KN-04, NEW-kernels-exec-88)
        return Ok(t);
    }
    memo.get(key, part, None, |t| t.table().ncodes() as usize == ncodes, || fp_table_as(part, attno, dict, over, bpchar))
}

/// `cond_table` through the statement's memo (`None`: this source's own, as before D3206). With the memo's execution under
/// `pgrust.condition_cache = off` or `granule` the table is the statement's alone: built once for its sources, never looked up in or
/// kept in the process cache, so no other statement reads it and this one reads none another built.
pub fn cond_table_in(memo: Option<&FpMemo>, part: &Arc<OpenPart>, attno: u32, spec: &TableSpec, dict: &DictView<'_>) -> Result<Arc<FpTable>, TableBuildError> {
    let Some(memo) = memo else { return cond_table(part, attno, spec, dict) };
    let key = ranks_key(part, attno, spec.fingerprint(), dict.ranks());
    if !memo.cross_statement() {
        return memo.get(key, part, Some(spec), |_| true, || build_cond_table(spec, dict, Held::By(dict.owner())).map(Arc::new)); // alloc: Exec
    }
    if let Some(t) = cached_for(&key, part, spec) {
        crate::ops::scan::pgrc::cond_cache::reuse::note(crate::ops::scan::pgrc::cond_cache::reuse::Kind::CodeTables, 1);
        return Ok(t);
    }
    memo.get(key, part, Some(spec), |_| true, || cond_table(part, attno, spec, dict))
}

/// `entry_runs` through the statement's memo (`None`: this source's own, as before D3206).
pub fn entry_runs_in(memo: Option<&FpMemo>, part: &Arc<OpenPart>, attno: u32, fp: &FpTable) -> Option<Arc<FpTable>> {
    let Some(memo) = memo else { return entry_runs(part, attno, fp) };
    let key = ranks_key(part, attno, crate::ops::scan::pgrc::entry_runs::RUNS_KEY, fp.ranks());
    if let Some(t) = cached(&key, part).filter(|t| t.runs().is_some()) {
        return Some(t);
    }
    memo.get(key, part, None, |t| t.runs().is_some(), || entry_runs(part, attno, fp).ok_or(())).ok()
}

/// `render_table` through the statement's memo (`None`: this source's own, as before D3206).
pub fn render_table_in(memo: Option<&FpMemo>, part: &Arc<OpenPart>, attno: u32, view: &DictView<'_>) -> Result<Arc<FpTable>, ReadError> {
    let Some(memo) = memo else { return render_table(part, attno, view) };
    let key = ranks_key(part, attno, RENDERS_KEY, view.ranks());
    if let Some(t) = cached(&key, part).filter(|t| t.renders) {
        return Ok(t);
    }
    memo.get(key, part, None, |t| t.renders, || render_table(part, attno, view))
}

/// The cache key's third word of a part's `Fp128` table: 0 the padded renders' (`TableFn::Fingerprint`), `FP_BPCHAR` the trimmed
/// bpchar form's (D767); both even, so neither is ever a condition table's key (its spec's fingerprint, always odd).
const FP_BPCHAR: u64 = 2;

/// The part's resident `Fp128` table, the padded (`bpchar` false) or the trimmed bpchar form (`TableFn::FingerprintBpchar`,
/// D767); never a build.
pub fn fp_resident_as(part: &Arc<OpenPart>, attno: u32, bpchar: bool) -> Option<Arc<FpTable>> {
    cached(&(part.uuid(), attno, if bpchar { FP_BPCHAR } else { 0 }), part)
}

/// The part's `Fp128` table over the dictionary of `attno`, the padded or the trimmed bpchar form (D767, each its own cache
/// entry): the resident one, else built now into its own buffer and kept in the process cache. The caller pins the `Arc` while
/// a lane points at the table.
pub fn fp_table_as(part: &Arc<OpenPart>, attno: u32, dict: &DictView<'_>, over: u8, bpchar: bool) -> Result<Arc<FpTable>, TableBuildError> {
    let key = (part.uuid(), attno, if bpchar { FP_BPCHAR } else { 0 });
    let ncodes = gravity_dict::Dict::ncodes(dict) as usize;
    if let Some(t) = fp_resident_as(part, attno, bpchar) {
        if t.table().ncodes() as usize == ncodes {
            fires::fire_shared("dict.fp_resident"); // shapecheck fire signal (REGISTRY H24, KN-04, NEW-kernels-exec-88)
            return Ok(t);
        }
        // a resident table of another dictionary under this part's key: dropped and rebuilt
        let mut g = FP_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = g.as_mut() {
            c.remove(&key);
        }
    }
    fp_seed();
    let spec = TableSpec { over, func: if bpchar { TableFn::FingerprintBpchar } else { TableFn::Fingerprint } };
    let mut words = (64 + 16 * ncodes + 64).div_ceil(8);
    let t = loop {
        // D4278: a resident table's buffer from the build pool, as a derived table's (its drop gives it back there); only
        // the buffer is the fp tables' (the build's working memory is not)
        let mut hb = take_held(words, Held::Resident);
        raw::advise_resident(&mut hb.buf);
        let mut region = raw::region(&mut hb.buf);
        match build_table(&spec, dict, &mut region, &mut None) {
            Ok(hdr) => break FpTable { cached: Mutex::new(hb.held), used: (hb.buf.len() * 8) as u64, buf: hb.buf, hdr, ranks: None, off: None, renders: false, pins: Mutex::new(None) },
            Err(TableBuildError::Region(gravity_dict::BuildError::RegionFull { needed })) => words = (words * 8 + needed + 64).div_ceil(8),
            Err(e) => return Err(e),
        }
    };
    FP_BUILT.fetch_add(1, Ordering::Relaxed);
    fires::fire_shared("dict.fp_build"); // shapecheck fire signal (REGISTRY H24, KN-04, NEW-kernels-exec-88)
    Ok(insert_fp(key, part, t).0)
}

fn cached(key: &([u8; 16], u32, u64), part: &Arc<OpenPart>) -> Option<Arc<FpTable>> {
    let t = {
        let mut g = FP_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let c = g.as_mut();
        if fp_cache_cap() == 0 {
            // a zero cap keeps nothing resident: every lookup misses (a restart's stand-in on a live server)
            if let Some(c) = c {
                c.remove(key);
            }
            None
        } else {
            c.and_then(|c| c.hit(key, part, None))
        }
    };
    missed(key, t)
}

/// A lookup of `key` that `t` answers: none arms the miss's timer (memledger::value), which the table's `insert_arc` ends.
#[inline]
fn missed(key: &([u8; 16], u32, u64), t: Option<Arc<FpTable>>) -> Option<Arc<FpTable>> {
    memledger::value::lookup(memledger::Consumer::FpTables, fp_value_key(key), 0);
    if t.is_none() {
        memledger::value::arm(memledger::Consumer::FpTables, fp_value_key(key));
        if memledger::value::ext() {
            if let Some(at) = FP_GHOSTS.take(fp_value_key(key)) {
                memledger::value::ghost(memledger::Consumer::FpTables, at, 1);
            }
        }
    }
    t
}

/// Broker (b): the ghosts of tables the cache evicted (least recently used): (key word, the cache's byte clock then).
static FP_GHOSTS: memledger::value::Ghosts<1024> = memledger::value::Ghosts::new();

/// Broker (b): the `used` tick at the cache's coldest tenth of bytes (`fp_publish_tail`, each broker plan); a hit on a table
/// last used at or below it is a tail hit. 0: none published.
static FP_TAIL_TICK: AtomicU64 = AtomicU64::new(0);

/// Ladder C1c: the tables an insert evicted to stay under the cache's budget, since start; C1d: their bytes (the ladder's
/// demand probe: a raise gives back what the cache evicted under its budget since the last one).
static FP_CAP_EVICTS: AtomicU64 = AtomicU64::new(0);
static FP_CAP_EVICTED_BYTES: AtomicU64 = AtomicU64::new(0);

pub fn fp_cap_evictions() -> u64 {
    FP_CAP_EVICTS.load(Ordering::Relaxed)
}

/// L7 (cache_fp_cap_pins): the inserts kept past the budget because every table left was held, and their bytes past it.
static FP_OVER_CAP_PINNED: AtomicU64 = AtomicU64::new(0);
static FP_OVER_CAP_PINNED_BYTES: AtomicU64 = AtomicU64::new(0);

pub fn fp_over_cap_pinned() -> (u64, u64) {
    (FP_OVER_CAP_PINNED.load(Ordering::Relaxed), FP_OVER_CAP_PINNED_BYTES.load(Ordering::Relaxed))
}

pub fn fp_cap_evicted_bytes() -> u64 {
    FP_CAP_EVICTED_BYTES.load(Ordering::Relaxed)
}

/// Broker (b): table `key`, `bytes` of it, evicted: the byte clock moves and the key keeps a ghost.
#[inline]
fn fp_evicted(key: &([u8; 16], u32, u64), bytes: u64) {
    if memledger::value::ext() {
        FP_GHOSTS.record(fp_value_key(key), memledger::value::evicted(memledger::Consumer::FpTables, bytes));
    }
}

/// Broker (b): publish the cache's tail, the `used` tick at its coldest tenth of bytes, and the bytes it covers. For the
/// broker's plan on the memory watchdog's thread: no allocation and no wait (`memledger::value::coldest_tenth` over the
/// tables under `try_lock`; busy, the last mark stays).
pub fn fp_publish_tail() {
    let Some(g) = memledger::value::try_lock(&FP_CACHE) else { return };
    let Some(c) = g.as_ref() else { return };
    let tail = memledger::value::coldest_tenth(c.map.len(), |f| {
        // bounded("other: on the memory watchdog's thread under try_lock, outside every statement's kernel call; one pass over the cache's entries")
        for e in c.map.values() {
            f.push(e.used, e.table.bytes());
        }
    });
    drop(g);
    if let Some((tick, bytes)) = tail {
        FP_TAIL_TICK.store(tick, Ordering::Relaxed);
        memledger::value::set_tail_bytes(memledger::Consumer::FpTables, bytes);
    }
}

/// The word a table's miss is timed by (memledger::value): its key.
#[inline]
fn fp_value_key(key: &([u8; 16], u32, u64)) -> u64 {
    let (lo, hi) = key.0.split_at(8);
    let w = |b: &[u8]| u64::from_le_bytes(b.try_into().unwrap_or([0; 8]));
    w(lo) ^ w(hi).rotate_left(29) ^ u64::from(key.1).rotate_left(48) ^ key.2.rotate_left(7)
}

/// A part's condition table (a LIKE, equality or range verdict per dictionary code): resident across statements under the
/// fingerprint cache's cap (R4), a cross-statement reuse `pgrust.condition_cache` governs (`cond_table_in`).
pub fn cond_table(part: &Arc<OpenPart>, attno: u32, spec: &TableSpec, dict: &DictView<'_>) -> Result<Arc<FpTable>, TableBuildError> {
    let key = ranks_key(part, attno, spec.fingerprint(), dict.ranks());
    if let Some(t) = cached_for(&key, part, spec) {
        crate::ops::scan::pgrc::cond_cache::reuse::note(crate::ops::scan::pgrc::cond_cache::reuse::Kind::CodeTables, 1);
        return Ok(t);
    }
    let t = build_cond_table(spec, dict, Held::Resident)?;
    FP_BUILT.fetch_add(1, Ordering::Relaxed);
    Ok(insert_arc(key, part, Arc::new(t), Some(spec)).0) // alloc: Cold
}

/// A condition table's build over `dict`, its buffer charged as `held` says (`cond_table`'s resident one, or the
/// statement's own).
fn build_cond_table(spec: &TableSpec, dict: &DictView<'_>, held: Held) -> Result<FpTable, TableBuildError> {
    let n = gravity_dict::Dict::ncodes(dict) as usize;
    let per_code = if spec.elem() == gravity_dict::lane::ELEM_BITS { n.div_ceil(8) } else { 8 * n };
    COND_BUILT.fetch_add(1, Ordering::Relaxed);
    build_owned_held(64 + per_code + (1 << 12), held, &mut |region| build_table(spec, dict, region, &mut None))
}

static COND_BUILT: AtomicU64 = AtomicU64::new(0);

/// The condition tables built so far in this process (`build_cond_table`): one per part and spec a statement builds.
pub fn cond_tables_built() -> u64 {
    COND_BUILT.load(Ordering::Relaxed)
}

/// `cached` of a table kept under `spec` (a condition table): a key held under another spec misses.
fn cached_for(key: &([u8; 16], u32, u64), part: &Arc<OpenPart>, spec: &TableSpec) -> Option<Arc<FpTable>> {
    let t = {
        let mut g = FP_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let c = g.as_mut();
        if fp_cache_cap() == 0 {
            if let Some(c) = c {
                c.remove(key);
            }
            None
        } else {
            c.and_then(|c| c.hit(key, part, Some(spec)))
        }
    };
    missed(key, t)
}

/// Keep a built table (the first insert of a key wins a race: `true` for the winner).
fn insert_fp(key: ([u8; 16], u32, u64), part: &Arc<OpenPart>, t: FpTable) -> (Arc<FpTable>, bool) {
    insert_arc(key, part, Arc::new(t), None) // alloc: Cold
}

/// `insert_fp` of a shared table, under `spec` for a condition one: a key `part` holds under another spec (a fingerprint
/// collision) keeps its table, and `t` goes back to the caller unkept; another part's entry under the key is replaced.
fn insert_arc(key: ([u8; 16], u32, u64), part: &Arc<OpenPart>, t: Arc<FpTable>, spec: Option<&TableSpec>) -> (Arc<FpTable>, bool) {
    let r = insert_arc_now(key, part, t, spec);
    // a table built or loaded: the fp cache's miss, the bytes it keeps (none when another builder's table stays)
    memledger::value::fault(memledger::Consumer::FpTables, fp_value_key(&key), if r.1 { r.0.bytes() } else { 0 });
    r
}

fn insert_arc_now(key: ([u8; 16], u32, u64), part: &Arc<OpenPart>, t: Arc<FpTable>, spec: Option<&TableSpec>) -> (Arc<FpTable>, bool) {
    let bytes = t.bytes();
    let cap = fp_cache_cap();
    FP_CAP_SEEN.store(cap, Ordering::Relaxed);
    // ladder C1b: under `cache_floor` the ladder sizes the fp tables (C1d: from the cap, raised past it on demand)
    let cap = mcx::ladder::sized_budget(mcx::ladder::SIZED_FP, cap);
    let mut g = FP_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let c = g.get_or_insert_with(FpCache::new);
    match c.owned_by(&key, part) {
        Some(true) => {
            let e = &c.map[&key];
            return if e.spec.as_ref() == spec { (Arc::clone(&e.table), false) } else { (t, false) };
        }
        Some(false) => {
            c.remove(&key);
            FP_RETIRED.fetch_add(1, Ordering::Relaxed);
        }
        None => {}
    }
    c.sweep_dead();
    // L7: under the ladder's budget a held table is in use, so the cap passes over it (cache_fp_cap_pins)
    let skip_held = mcx::ladder::floors_on() && mempolicy::consts::get(mempolicy::consts::K::CacheFpCapPins) != 0;
    // bounded("other: evicts the least recently used tables until the new one fits the cache's byte cap, one table a pass; the cap bounds the entries")
    while c.bytes + bytes > cap && !c.map.is_empty() {
        let Some(oldest) = c.map.iter().filter(|(_, e)| !skip_held || Arc::strong_count(&e.table) == 1).min_by_key(|(_, e)| e.used).map(|(k, _)| *k) else {
            FP_OVER_CAP_PINNED.fetch_add(1, Ordering::Relaxed);
            FP_OVER_CAP_PINNED_BYTES.fetch_add((c.bytes + bytes - cap).min(bytes), Ordering::Relaxed);
            break;
        };
        let gone = c.map[&oldest].table.bytes();
        fp_evicted(&oldest, gone);
        c.remove(&oldest);
        FP_CAP_EVICTS.fetch_add(1, Ordering::Relaxed);
        FP_CAP_EVICTED_BYTES.fetch_add(gone, Ordering::Relaxed);
    }
    c.tick += 1;
    c.bytes += bytes;
    t.take_into_cache();
    let (reuse_epoch, reuse_secs) = match pgrc_read::partcache::now_sec() {
        0 => (0, 0),
        now => (pgrc_read::partcache::statement_epoch(), u64::from(now)),
    };
    let e = FpEntry { table: Arc::clone(&t), part: Arc::downgrade(part), used: c.tick, spec: spec.cloned(), reuse_epoch, reuse_secs }; // alloc: Cold charged FP_OWNER (FpCache::charge: the spec's bytes, refit below)
    c.specs += e.spec_bytes();
    c.map.insert(key, e); // alloc: Cold charged FP_OWNER (FpCache::charge: the map's slots, refit below)
    c.refit();
    (t, true)
}

/// The part's entry runs over its `Fp128` table `fp` of `attno` (granule_source/entry_runs.rs, road dict.entries' runs): resident in
/// the fingerprint cache exactly like the part's `Fp128` table (keyed `(uuid, attno, RUNS_KEY)`: the part's generation, the
/// same cap and LRU, the dead-part sweep, `retire_part`, `fp_cache_drop_all`), built from `fp` on a miss.
/// `None`: `fp` holds no plain entries (faulted, or not an `Fp128` table): the part publishes its rows.
pub fn entry_runs(part: &Arc<OpenPart>, attno: u32, fp: &FpTable) -> Option<Arc<FpTable>> {
    let key = ranks_key(part, attno, crate::ops::scan::pgrc::entry_runs::RUNS_KEY, fp.ranks());
    if let Some(t) = cached(&key, part).filter(|t| t.runs().is_some()) {
        return Some(t);
    }
    // a cancel publishes the part's rows (`None`), as a table without plain entries does; the walk's next tick stops
    let (buf, off) = crate::ops::scan::pgrc::entry_runs::build(fp.table_entries()?).ok()?;
    FP_BUILT.fetch_add(1, Ordering::Relaxed);
    Some(insert_fp(key, part, FpTable::of_runs(buf, off)).0)
}

/// D4329: the least a build's working memory charges to the grant pool at once (`ScratchGate::cover`).
pub const SCRATCH_STEP: u64 = 1 << 20;

/// D4329: a verbatim part's dictionary build's working memory (`row_codes`' copy of the column and its row arrays,
/// `render_table`'s images; heap outside every region, on the scan's threads) charged to the grant pool before the build
/// grows (`gravity_driver::mem::region::scratch_draw`: the claim's purse, the statement's part of the pool, else an
/// overdraft past the pool, `Overdraw::Scratch`). PostgreSQL holds no such memory, so it is never refused (a refusal would
/// be a 53200 PostgreSQL never raises) and never waits; held in the pool, admission and the statements beside it see it.
/// It holds the most the build held (a high-water mark) and gives it back at its drop, the build's end; a finished table
/// then moves to the fingerprint cache's own charge.
pub struct ScratchGate {
    /// D4537 7: on the building statement's owner (`charge::context`).
    held: memledger::ChargedVec<gravity_driver::Held>,
    drawn: u64,
}

impl Default for ScratchGate {
    fn default() -> Self {
        ScratchGate::new()
    }
}

impl ScratchGate {
    pub fn new() -> ScratchGate {
        ScratchGate { held: memledger::ChargedVec::new(Some(memledger::charge::context().0), memledger::Kind::Region), drawn: 0 } // alloc: Cold charged context() (held: a ChargedVec, empty until cover)
    }

    /// The bytes drawn so far.
    pub fn drawn(&self) -> u64 {
        self.drawn
    }

    /// The build is about to hold `bytes` at once: the part past what it drew is charged now, at least a quarter more than
    /// it holds and `SCRATCH_STEP` (a doubling column charges a few times, not at every growth).
    pub fn cover(&mut self, bytes: u64) {
        if bytes <= self.drawn {
            return;
        }
        let ask = (bytes - self.drawn).max(self.drawn / 4).max(SCRATCH_STEP);
        // a refused push gives the draw back at once (its drop): the next cover asks again
        if self.held.push(gravity_driver::mem::region::scratch_draw(ask)).is_ok() { // alloc: Cold charged context() (held's ChargedVec)
            self.drawn += ask;
        }
    }

    /// `cover` for `v` growing by `more` elements beside `live` bytes the build holds now: nothing while its capacity holds,
    /// else `live` and the grown buffer (`cancelpoll::Step::reserve`'s size), the old one still live during the move.
    pub fn cover_growth<T>(&mut self, v: &Vec<T>, more: usize, live: u64) {
        if v.capacity() - v.len() >= more {
            return;
        }
        let grown = (v.len() + more).max(v.capacity().saturating_mul(2)) * core::mem::size_of::<T>();
        self.cover(live.saturating_add(grown as u64));
    }
}

/// D4329: the bytes of `v`'s buffer.
fn cap_bytes<T>(v: &Vec<T>) -> u64 {
    (v.capacity() * core::mem::size_of::<T>()) as u64
}

/// The cache key's third word of a verbatim part's rendered images (`render_table`): even, never a condition table's.
pub const RENDERS_KEY: u64 = u64::from_le_bytes(*b"RENDERS0");

/// A verbatim part's renders (`RenderMemo`): every code with a live representative row, its image a 4B-U varlena in one
/// buffer, resident in the fingerprint cache like the part's code lane (keyed `(uuid, attno, RENDERS_KEY)`: the same cap,
/// LRU and dead-part sweep, `retire_part`, `fp_cache_drop_all`), so every participant and statement that renders the
/// part reads one copy. Built from `view` (the part's rows through the caller's cursor) on a miss. A read error is the
/// view's.
pub fn render_table(part: &Arc<OpenPart>, attno: u32, view: &DictView<'_>) -> Result<Arc<FpTable>, ReadError> {
    let key = ranks_key(part, attno, RENDERS_KEY, view.ranks());
    if let Some(t) = cached(&key, part).filter(|t| t.renders) {
        return Ok(t);
    }
    let n = gravity_dict::Dict::ncodes(view) as usize;
    let cap = view.ranks().map_or(n, |r| r.render_words.max(n)).max(1);
    // D4329: the images and their offsets charged to the grant pool before they grow, given back at the build's end
    let mut gate = ScratchGate::new();
    // D4321 step 1, item 2 (LADDER-DESIGN 6): the render images are a cache image: laid on the cache arena, charged to the
    // fp tables as they are taken (`lay_renders`)
    let laid = lay_renders(view, n, cap, &mut gate);
    let (DictView::Part(_, err) | DictView::Ranked { err, .. }) = view;
    if let Some(e) = err.take() {
        return Err(e);
    }
    let (taken, buf, off) = laid?;
    if view.ranks().is_some_and(|r| r.render_words != 0) && buf.len() <= cap {
        fires::fire_shared("dict.render_sized"); // shapecheck fire signal (D3232): the render table was laid out in one allocation
    }
    FP_BUILT.fetch_add(1, Ordering::Relaxed);
    let mut t = FpTable::of_renders(buf, off);
    *t.cached.get_mut().unwrap_or_else(|e| e.into_inner()) = taken;
    Ok(insert_fp(key, part, t).0)
}

/// `render_table`'s images of `d`'s `n` codes: each code with a live representative row, its image a 4B-U varlena, in one
/// buffer of `cap` words at first; and the word each code's image starts at (`u32::MAX`: none).
/// C9 (D4233): the walk ticks per code, and a code's image is laid `PIECE` bytes a tick here (its pages fault in as
/// they are written: one long value took 258 ms between two of the walk's ticks); so are the buffer's growth and the
/// offsets' fill. A stop for the statement's cancel drops the partial images (`ReadError::Canceled`). D4329: the offsets
/// and the images are charged to the grant pool (`gate`) before they grow. CACHE-CHARGE: each allocation of the offsets
/// and the images (and nothing else: the walk's memory is its source's) is taken under the fp tables' cache scope, and
/// the scopes' charges are one (the first of the result: it ends before the images are freed).
fn lay_renders(d: &dyn gravity_dict::Dict, n: usize, cap: usize, gate: &mut ScratchGate) -> Result<(Option<memledger::Charge>, Vec<u64>, Box<[u32]>), ReadError> {
    let mut step = cancelpoll::step("text_hosts render_table: a verbatim part's render images laid out");
    let stop = |_: cancelpoll::Canceled| ReadError::Canceled;
    gate.cover((n * 4 + cap * 8) as u64);
    let mut off: Vec<u32> = Vec::new(); // alloc: Cold
    let mut buf: Vec<u64> = Vec::new(); // alloc: Cold
    let mut taken: Option<memledger::Charge> = None;
    let scope = memledger::arena::cache_scope_for(FP_OWNER, memledger::Kind::Region);
    let reserved = step.reserve(&mut off, n);
    buf.reserve_exact(cap); // alloc: Cold charged FP_OWNER
    adopt(&mut taken, scope.finish());
    reserved.map_err(stop)?;
    step.pieces(n, cancelpoll::PIECE / 4, |r| {
        off.resize(r.end, u32::MAX); // alloc: Cold charged FP_OWNER
        Ok::<(), cancelpoll::Canceled>(())
    })
    .map_err(stop)?;
    let mut stopped: Option<ReadError> = None;
    let mut put = |code: u32, v: gravity_dict::View| {
        if stopped.is_some() {
            return;
        }
        let b = crate::ops::scan::pgrc::raw::walk_bytes(&v);
        let (at, words) = (buf.len(), (4 + b.len()).div_ceil(8));
        gate.cover_growth(&buf, words, cap_bytes(&off) + cap_bytes(&buf));
        // a growth of the images (a new buffer, the old one freed) under the scope: its charge moves by the difference
        let scope = (buf.capacity() - buf.len() < words).then(|| memledger::arena::cache_scope_for(FP_OWNER, memledger::Kind::Region));
        let zeroed = step.reserve(&mut buf, words).and_then(|()| {
            step.pieces(words, cancelpoll::PIECE / 8, |r| {
                buf.resize(at + r.end, 0); // alloc: Cold charged FP_OWNER
                Ok::<(), cancelpoll::Canceled>(())
            })
        });
        if let Some(scope) = scope {
            adopt(&mut taken, scope.finish());
        }
        if zeroed.is_err() {
            stopped = Some(ReadError::Canceled);
            return;
        }
        let img = crate::ops::scan::pgrc::raw::words_as_bytes_mut(&mut buf[at..at + words]);
        img[..4].copy_from_slice(&((b.len() as u32 + 4) << 2).to_le_bytes());
        let copied = step.pieces(b.len(), cancelpoll::PIECE, |r| {
            img[4 + r.start..4 + r.end].copy_from_slice(&b[r]);
            Ok::<(), cancelpoll::Canceled>(())
        });
        if copied.is_err() {
            stopped = Some(ReadError::Canceled);
        } else if let Some(o) = off.get_mut(code as usize) {
            *o = at as u32;
        }
    };
    d.entries(&mut put);
    if let Some(e) = stopped {
        return Err(e);
    }
    Ok((taken, buf, off.into_boxed_slice()))
}

/// D4732 (a): the most `row_codes` builds in flight at once that single-flight tracks (a thread builds one at a time, so
/// this is past every pool's threads); a build that finds every slot taken builds untracked, as before.
const FLIGHT_SLOTS: usize = 64;

/// D4732 (a): the keys whose table a `row_codes` build is making now. A concurrent opener of the same key (another worker
/// of the statement in the same part's claims, the end-game's or skewsplit's split of a running claim, another session's
/// statement) waits for that build (`fp_flight`) and reads the table it keeps, instead of decoding and sorting the same
/// part's column beside it: the tail rule's cut parts had every worker of a W8 statement rank each one cold (a verbatim
/// text column's parts built 2.6x as often, 3.4x the statement's CPU). Slots in place: no allocation.
static FP_FLIGHT: Mutex<[Option<([u8; 16], u32, u64)>; FLIGHT_SLOTS]> = Mutex::new([None; FLIGHT_SLOTS]);
static FP_FLIGHT_CV: pgsync::Condvar = pgsync::Condvar::new();
/// The openers that waited on another's build of the same table, since start.
static FP_FLIGHT_WAITS: AtomicU64 = AtomicU64::new(0);

/// One slice of a wait on another opener's build (`fp_flight`): the waiter ticks its step between slices, so the
/// statement's cancel ends the wait within one slice (the openpart in-flight read wait's 10 ms).
const FLIGHT_SLICE: core::time::Duration = core::time::Duration::from_millis(10);

/// The openers that waited on another's build of the same table, since start (single-flight, D4732).
pub fn fp_flight_waits() -> u64 {
    FP_FLIGHT_WAITS.load(Ordering::Relaxed)
}

/// A build in flight (`fp_flight`): its slot is freed and its waiters woken at the drop, which follows the table's insert
/// on success and comes first on an error, a cancel or an unwind (a waiter then builds the table itself).
struct Flight(usize);

impl Drop for Flight {
    fn drop(&mut self) {
        FP_FLIGHT.lock().unwrap_or_else(|e| e.into_inner())[self.0] = None;
        FP_FLIGHT_CV.notify_all();
    }
}

/// D4732 (a): whether part `part`'s whole ranks table of `attno` (`row_codes`' key with no kept granules) is resident now:
/// a look, no LRU touch (the claims' tail rule asks it of the parts it would cut, exec runner/plan.rs `claims_of`).
pub fn ranks_resident(part: &Arc<OpenPart>, attno: u32) -> bool {
    let g = FP_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    g.as_ref().is_some_and(|c| c.owned_by(&(part.uuid(), attno, 0), part) == Some(true))
}

/// D4732 (a), a test's stand-in for a build in flight: part `part`'s whole table of `attno` (`row_codes`' key with no kept
/// granules) held as a build holds it, so openers of it wait; the drop ends the flight with no table kept, as a build's
/// error does. `None` while another build holds the key or every slot is taken.
#[doc(hidden)]
pub struct FlightHold(#[allow(dead_code)] Flight);

#[doc(hidden)]
pub fn hold_row_codes_flight(part: &Arc<OpenPart>, attno: u32) -> Option<FlightHold> {
    let key = (part.uuid(), attno, 0);
    let mut g = FP_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    if g.iter().flatten().any(|k| *k == key) {
        return None;
    }
    let i = g.iter().position(Option::is_none)?;
    g[i] = Some(key);
    Some(FlightHold(Flight(i)))
}

/// `key`'s build: `Ok(Some(_))` when no other opener builds it now (the caller builds under the flight; `Some(None)` when
/// every slot is taken: the caller builds untracked), `Ok(None)` after another opener's build of it ended (the caller
/// looks in the cache again), `Err` for the statement's cancel during the wait. The wait is bounded by the builder's own
/// polled build; the builder never waits on anything its waiters hold (a build reads only its own part).
fn fp_flight(key: ([u8; 16], u32, u64)) -> Result<Option<Option<Flight>>, cancelpoll::Canceled> {
    let mut g = FP_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    if g.iter().flatten().any(|k| *k == key) {
        FP_FLIGHT_WAITS.fetch_add(1, Ordering::Relaxed);
        let mut step = cancelpoll::step("text_hosts row_codes: a wait on another opener's build of the same part's ranks");
        while g.iter().flatten().any(|k| *k == key) {
            g = FP_FLIGHT_CV.wait_timeout(g, FLIGHT_SLICE).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0);
            step.tick()?;
        }
        return Ok(None);
    }
    Ok(Some(g.iter().position(Option::is_none).map(|i| {
        g[i] = Some(key);
        Flight(i)
    })))
}

/// A verbatim part's code lane with its `Fp128` table: the resident one, else built now — one decode
/// of the column (`cur`), the live rows sorted by bytes, one code per distinct value, one
/// fingerprint per code — and kept under the fingerprint cache's cap.
/// D3156: with `kept`, the table of those granules' rows alone (`Ranks::covers`): the others are neither read nor coded;
/// a resident whole-part table serves it as it is (it covers every granule).
/// D4732 (a): one build per table at a time (`fp_flight`): a concurrent opener of the same table waits for it and reads
/// the table it kept.
pub fn row_codes(part: &Arc<OpenPart>, cur: &mut StreamCursor<'static>, attno: u32, scratch: &mut TextScratch, kept: Option<&Kept>) -> Result<Arc<FpTable>, TableBuildError> {
    let whole = (part.uuid(), attno, 0);
    let key = kept.map_or(whole, |k| (part.uuid(), attno | KEPT_BIT, k.salt));
    let same = |t: &Arc<FpTable>| t.ranks().is_some_and(|r| r.kept.as_deref() == kept.map(|k| &*k.bits));
    let _flight = loop {
        {
            let mut g = FP_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = g.as_mut() {
                if let Some(t) = c.hit(&key, part, None).filter(same) {
                    return Ok(t);
                }
                if let Some(t) = kept.and_then(|_| c.hit(&whole, part, None)).filter(|t| t.ranks().is_some_and(|r| r.kept.is_none())) {
                    return Ok(t);
                }
            }
        }
        if let Some(f) = fp_flight(key).map_err(|_| TableBuildError::Read(ReadError::Canceled))? {
            break f;
        }
    };
    let granules = part.footer().granule_count;
    // D4329: the build's copy of the column, its row arrays and its table charged to the grant pool before they grow
    // (`ScratchGate`), given back at the build's end; past the statement's part of the pool, an overdraft (never refused)
    let mut gate = ScratchGate::new();
    gate.cover(4 * (granules as u64 + 1));
    // the column's copy, its row entries, the order and the sort's scratch: the build's working memory (freed at its end),
    // each on its own arena charged to the breaker the source's pipeline feeds (`TextScratch::set_owner`)
    let owner = scratch.owner;
    let mut store: Grow<u8> = Grow::new(owner);
    let mut ents: Grow<(u32, u32)> = Grow::new(owner);
    // the granules' row starts go into the cached table: the fp tables' from their allocation (CACHE-CHARGE: on the cache
    // arena, charged to its holder; exact, so no push regrows them); `taken` ends before they are freed
    let scope = memledger::arena::cache_scope_for(FP_OWNER, memledger::Kind::Region);
    let mut starts: Vec<u32> = Vec::with_capacity(granules as usize + 1); // alloc: Cold charged FP_OWNER
    let mut taken = scope.finish();
    let (mut row, mut live_rows) = (0u32, 0usize);
    // D3887 (C9): a tick per granule decoded (the whole column is decoded before its first code), the rows' sort polled
    // (`cancelpoll::sort_by`: seconds of memcmp on a verbatim part, the D3821 stress's 3 s cancel lag), a batch of codes.
    // D4232: a tick per row's payload copied and per `PIECE` of a long one, and the copies' growth polled (a granule's
    // copies and a doubling of the column's bytes faulted in 16k pages between two granule ticks)
    let mut step = cancelpoll::step("text_hosts row_codes: a verbatim part's rows ranked by their bytes");
    let stop = |_: cancelpoll::Canceled| TableBuildError::Read(ReadError::Canceled);
    for lg in 0..granules {
        starts.push(row); // alloc: Cold charged FP_OWNER
        if kept.is_some_and(|k| !k.covers(lg)) {
            continue;
        }
        step.tick().map_err(stop)?;
        let (rows, all) = scratch.decode(cur, lg).map_err(TableBuildError::Read)?;
        let live = store.cap_bytes() + ents.cap_bytes() + cap_bytes(&starts);
        ents.cover_growth(&mut gate, rows, live);
        ents.reserve(&mut step, rows)?;
        for i in 0..rows {
            if !scratch.live(all, i) {
                ents.push((u32::MAX, 0))?;
                continue;
            }
            let b = raw::payload(scratch.datums[i]);
            ents.push((store.len() as u32, b.len() as u32))?;
            live_rows += 1;
            if store.capacity() - store.len() < b.len() {
                store.cover_growth(&mut gate, b.len(), store.cap_bytes() + ents.cap_bytes() + cap_bytes(&starts));
            }
            store.reserve(&mut step, b.len())?;
            step.pieces(b.len(), cancelpoll::PIECE, |r| store.extend_from_slice(&b[r]))?;
        }
        row += rows as u32;
    }
    starts.push(row); // alloc: Cold charged FP_OWNER
    let (store_b, ents_s) = (store.as_slice(), ents.as_slice());
    let bytes_of = |e: &(u32, u32)| &store_b[e.0 as usize..(e.0 + e.1) as usize];
    // the order, its sort's scratch, the codes, and the distinct values' two arrays (at most one entry a row) beside the copy
    let n = ents_s.len() as u64;
    let sort_scratch = if ents_s.len() > cancelpoll::SORT_RUN { 4 * n } else { 0 };
    gate.cover(store.cap_bytes() + ents.cap_bytes() + cap_bytes(&starts) + 4 * n + sort_scratch + 4 * n + 12 * n);
    // the live rows counted in the fill: the order reserved whole, the sort's scratch on an arena dropped after the sort
    let order_arena = arena_of(owner);
    let mut order: AVec<u32> = AVec::with_capacity(&order_arena, live_rows).map_err(refused)?;
    for (i, e) in ents_s.iter().enumerate() {
        step.batch(i).map_err(stop)?;
        if e.0 != u32::MAX {
            order.push(&order_arena, i as u32).map_err(refused)?;
        }
    }
    {
        let sort_arena = arena_of(owner);
        let mut scratch_order: AVec<u32> = AVec::default();
        if order.len() > cancelpoll::SORT_RUN {
            scratch_order.resize(&sort_arena, order.len(), 0).map_err(refused)?;
        }
        cancelpoll::sort_by(&mut step, order.as_mut_slice(&order_arena), scratch_order.as_mut_slice(&sort_arena), |&a, &b| bytes_of(&ents_s[a as usize]).cmp(bytes_of(&ents_s[b as usize])))
            .map_err(|_| TableBuildError::Read(ReadError::Canceled))?;
    }
    let work = store.cap_bytes() + ents.cap_bytes();
    let (order, ents) = (order.as_slice(&order_arena), ents_s);
    // D4321 step 1, item 2 (LADDER-DESIGN 6): what the cached table keeps (its codes, representatives and table buffer)
    // is allocated on the cache arena from here to the build's end, CACHE-CHARGE: charged to the fp tables as it is taken
    // (`cache_scope_for`); the distinct values, freed inside the scope, leave its charge. The column's copy and the sort's
    // arrays above stay on the scratch arenas (hundreds of MB, freed at the build's end)
    let mut scope = memledger::arena::cache_scope_for(FP_OWNER, memledger::Kind::Region);
    let mut codes = vec![0u32; ents.len()]; // alloc: Cold charged FP_OWNER
    let mut reps: Vec<u32> = Vec::new(); // alloc: Cold charged FP_OWNER
    let mut distinct: Vec<(u32, u32)> = Vec::new(); // alloc: Cold charged FP_OWNER
    for (k, &i) in order.iter().enumerate() {
        step.batch(k).map_err(|_| TableBuildError::Read(ReadError::Canceled))?;
        let e = ents[i as usize];
        if reps.last().is_none_or(|&r| bytes_of(&ents[r as usize]) != bytes_of(&e)) {
            if reps.len() >= 1 << gravity_abi::status::FAULT_CODE_BITS {
                return Err(TableBuildError::Region(gravity_dict::BuildError::TooManyCodes));
            }
            reps.push(i);
            distinct.push(e);
        }
        codes[i as usize] = reps.len() as u32 - 1;
    }
    let empty = distinct.first().is_some_and(|e| e.1 == 0);
    let render_words = distinct.iter().map(|e| (4 + e.1 as usize).div_ceil(8)).sum();
    let dict = Store { bytes: store_b, ents: &distinct, stopped: core::cell::Cell::new(false) };
    let mut words = (64 + 16 * distinct.len() + 64).div_ceil(8);
    fp_seed();
    // bounded("other: a retry only when the region was short, grown by what the build asked for; each build's walk polls (Store's stopped flag)")
    let mut t = loop {
        gate.cover(work + cap_bytes(&starts) + 4 * n + cap_bytes(&codes) + cap_bytes(&reps) + cap_bytes(&distinct) + 8 * words as u64);
        let mut buf = reuse::take_for(words, Some(&mut scope)); // D4278: from the build pool
        buf.own(FP_OWNER);
        raw::advise_resident(&mut buf);
        let mut region = raw::region(&mut buf);
        match gravity_dict::Fp128::build(&dict, &mut region, &mut |v| Ok(v)) {
            // D3887: the walk stopped for the statement's cancel: the partial table is dropped
            Ok(_) if dict.stopped.get() => return Err(TableBuildError::Read(ReadError::Canceled)),
            Ok(t) => {
                let ranks = Ranks { codes, starts, reps, empty, kept: kept.map(|k| k.bits.clone()), salt: kept.map_or(0, |k| k.salt), lens: None, render_words }; // alloc: Cold
                break FpTable { cached: Mutex::new(None), used: (buf.len() * 8) as u64, buf, hdr: t.header(), ranks: Some(ranks), off: None, renders: false, pins: Mutex::new(None) };
            }
            Err(gravity_dict::BuildError::RegionFull { needed }) => words = (words * 8 + needed + 64).div_ceil(8),
            Err(e) => return Err(TableBuildError::Region(e)),
        }
    };
    // the distinct values freed inside the scope (they leave its charge); what it took is the table's, with the starts'
    drop(distinct);
    adopt(&mut taken, scope.finish());
    *t.cached.get_mut().unwrap_or_else(|e| e.into_inner()) = taken;
    FP_BUILT.fetch_add(1, Ordering::Relaxed);
    Ok(insert_fp(key, part, t).0)
}

/// D3038 (A): the cache salt of a table coded by length (`lane_codes`): even, so never a kept-granule table's (`Kept` salts
/// are odd), and not 0 (the whole part's byte-ranked table).
const LENS_SALT: u64 = 0x4c45_4e53_434f_4445 & !1;

/// D3038 (A): a verbatim part's code lane coded by length, for a lane its site reads only through `OCTET_LENGTH` tables and
/// the emptiness test (recipe `lens_only`): read from `cur`, the part's sealed octet-length lane of column `attno` (an int4
/// word a row, valid where the column is not NULL), never from the column's text. Code `c` is the part's `c`-th distinct
/// byte length, ascending (code 0 = length 0 when a row is empty, so the window's empty code is the emptiness test's), a
/// NULL row's code 0, and `Ranks::by_length` holds each code's length (the `OCTET_LENGTH` table's). The `Fp128` table is
/// over each length's 4-byte big-endian image only: no fingerprint, render or byte-ordered read may use a table coded so
/// (`build_table_inner` and the entries walk refuse it). Cached under `KEPT_BIT` and `LENS_SALT`, apart from the part's
/// byte-ranked table and its kept-granule ones.
pub fn lane_codes(part: &Arc<OpenPart>, attno: u32, cur: &mut StreamCursor<'static>) -> Result<Arc<FpTable>, TableBuildError> {
    let key = (part.uuid(), attno | KEPT_BIT, LENS_SALT);
    {
        let mut g = FP_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = g.as_mut().and_then(|c| c.hit(&key, part, None)).filter(|t| t.ranks().is_some_and(|r| r.lens.is_some())) {
            return Ok(t);
        }
    }
    let granules = part.footer().granule_count;
    // D4321 step 1, item 2 (LADDER-DESIGN 6): the length-coded table is a cache image built from the part's small
    // octet-length lane: its whole build allocates on the cache arena, CACHE-CHARGE: charged to the fp tables as it is
    // taken (`cache_scope_for`); the build's temporaries, freed inside the scope before it ends, leave its charge
    let mut scope = memledger::arena::cache_scope_for(FP_OWNER, memledger::Kind::Region);
    let mut lens: Vec<u32> = Vec::new(); // alloc: Cold
    let mut starts: Vec<u32> = Vec::with_capacity(granules as usize + 1); // alloc: Cold
    let (mut words, mut valid): (Vec<i32>, Vec<u64>) = (Vec::new(), Vec::new()); // alloc: Cold
    // D3887 (C9): a tick per granule decoded
    let mut step = cancelpoll::step("text_hosts lane_codes: a verbatim part's lengths coded");
    for lg in 0..granules {
        starts.push(lens.len() as u32);
        step.tick().map_err(|_| TableBuildError::Read(ReadError::Canceled))?;
        let rows = cur.rows_in_granule(lg) as usize;
        words.resize(rows, 0); // alloc: Cold
        valid.resize(rows.div_ceil(64), 0); // alloc: Cold
        if cur.decode_i32(lg, &mut words).map_err(TableBuildError::Read)? as usize != rows {
            return Err(TableBuildError::Read(ReadError::Format(FormatError::Corrupt { at: "length codes: decoded count" })));
        }
        let all = !cur.has_validity_stream() || matches!(cur.validity(lg, &mut valid).map_err(TableBuildError::Read)?, ValidityVerdict::AllValid);
        // bounded("small-fixed: one granule's rows (at most GRANULE_ROWS, 8192), a length each; the granule loop ticks")
        for (i, &w) in words.iter().enumerate() {
            let live = all || (valid[i / 64] >> (i % 64)) & 1 != 0;
            // a NULL row's length is no code's: its code is 0, as a byte-ranked table's (its validity is the column's)
            lens.push(if live { u32::try_from(w).map_err(|_| TableBuildError::Read(ReadError::Format(FormatError::Corrupt { at: "length codes: a negative length" })))? } else { u32::MAX });
        }
    }
    starts.push(lens.len() as u32);
    // the part's distinct lengths (a part's rows are millions of keys): each SORT_RUN rows' lengths sorted and deduplicated
    // under a tick, then the runs' survivors (a few per run) sorted under the step
    let mut distinct: Vec<u32> = Vec::new(); // alloc: Cold
    let mut run: Vec<u32> = Vec::with_capacity(cancelpoll::SORT_RUN.min(lens.len())); // alloc: Cold
    for rows in lens.chunks(cancelpoll::SORT_RUN) {
        step.tick().map_err(|_| TableBuildError::Read(ReadError::Canceled))?;
        run.clear();
        run.extend(rows.iter().copied().filter(|&l| l != u32::MAX)); // alloc: Cold
        // bounded("small-fixed: at most SORT_RUN (64Ki) lengths, an integer sort of about a millisecond; the run loop ticks")
        run.sort_unstable();
        run.dedup();
        distinct.extend_from_slice(&run); // alloc: Cold
    }
    drop(run);
    let mut merged: Vec<u32> = if distinct.len() > cancelpoll::SORT_RUN { vec![0; distinct.len()] } else { Vec::new() }; // alloc: Cold
    cancelpoll::sort_by(&mut step, &mut distinct, &mut merged, |a, b| a.cmp(b)).map_err(|_| TableBuildError::Read(ReadError::Canceled))?;
    drop(merged);
    distinct.dedup();
    if distinct.len() >= 1 << gravity_abi::status::FAULT_CODE_BITS {
        return Err(TableBuildError::Region(gravity_dict::BuildError::TooManyCodes));
    }
    let mut reps = vec![u32::MAX; distinct.len()]; // alloc: Cold
    // a row a binary search over the distinct lengths: a batch of rows a tick
    let mut codes: Vec<u32> = Vec::with_capacity(lens.len()); // alloc: Cold
    for (row, &l) in lens.iter().enumerate() {
        step.batch(row).map_err(|_| TableBuildError::Read(ReadError::Canceled))?;
        if l == u32::MAX {
            codes.push(0);
            continue;
        }
        let c = distinct.binary_search(&l).expect("a distinct length") as u32;
        if reps[c as usize] == u32::MAX {
            reps[c as usize] = row as u32;
        }
        codes.push(c);
    }
    let empty = distinct.first() == Some(&0);
    // the Fp128 table over the lengths' images: a table coded so is never fingerprinted (only the shape needs it)
    let store: Vec<u8> = distinct.iter().flat_map(|l| l.to_be_bytes()).collect(); // alloc: Cold
    let ents: Vec<(u32, u32)> = (0..distinct.len() as u32).map(|c| (4 * c, 4)).collect(); // alloc: Cold
    let dict = Store { bytes: &store, ents: &ents, stopped: core::cell::Cell::new(false) };
    let mut fpw = (64 + 16 * ents.len() + 64).div_ceil(8);
    fp_seed();
    // bounded("other: a retry only when the region was short, grown by what the build asked for; each build's walk polls (Store's stopped flag)")
    let mut t = loop {
        let mut buf = reuse::take_for(fpw, Some(&mut scope)); // D4278: from the build pool
        buf.own(FP_OWNER);
        let mut region = raw::region(&mut buf);
        match gravity_dict::Fp128::build(&dict, &mut region, &mut |v| Ok(v)) {
            // D3887: the walk stopped for the statement's cancel: the partial table is dropped
            Ok(_) if dict.stopped.get() => return Err(TableBuildError::Read(ReadError::Canceled)),
            Ok(t) => {
                let lens = Some(distinct.iter().map(|&l| l as i64).collect()); // alloc: Cold
                let ranks = Ranks { codes, starts, reps, empty, kept: None, salt: LENS_SALT, lens, render_words: 0 };
                break FpTable { cached: Mutex::new(None), used: (buf.len() * 8) as u64, buf, hdr: t.header(), ranks: Some(ranks), off: None, renders: false, pins: Mutex::new(None) };
            }
            Err(gravity_dict::BuildError::RegionFull { needed }) => fpw = (fpw * 8 + needed + 64).div_ceil(8),
            Err(e) => return Err(TableBuildError::Region(e)),
        }
    };
    // the build's temporaries freed inside the scope (they leave its charge); what it took is the table's
    drop((store, ents, distinct, lens, words, valid));
    *t.cached.get_mut().unwrap_or_else(|e| e.into_inner()) = scope.finish();
    FP_BUILT.fetch_add(1, Ordering::Relaxed);
    Ok(insert_fp(key, part, t).0)
}

/// The per-entry function of a table lane (expr.md §2.2; the kernel author's dependency split): what
/// the source evaluates once per dictionary entry of the code lane `over`, published as table lane `k`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TableFn {
    /// `LIKE` / `ILIKE` (`negated`: `NOT LIKE`) in the backslash convention; a `Bits` table.
    Like { pattern: Vec<u8>, ilike: bool, negated: bool }, // alloc: Program
    /// `octet_length(text)`: an `I64` table of byte lengths.
    OctetLength,
    /// `length(text)` under UTF-8: an `I64` table of character counts.
    CharLength,
    /// The entry's 128-bit fingerprint (`gravity_dict::fp`, keyed SipHash-2-4-128 under the process seed): an `Fp128` table.
    Fingerprint,
    /// `Fingerprint` of the entry without its trailing blanks (`bcTruelen`: `hashbpchar`'s image), a bpchar code lane's
    /// (`table_fn::FLAG_BPCHAR`; D767): 'a' and 'a  ' one `Fp128`, equal to a heap bpchar key's image. Its own cache entry
    /// (`fp_table_as`).
    FingerprintBpchar,
    /// textkeys step 3 (`table_fn::FLAG_CANON` on `FINGERPRINT`, lowerv4 road `key.canon`): each entry's canonical text key
    /// image (`gravity_dict::fp::canon_of`: its bytes up to 15, else its Fp128 under bit 63) in the `Fp128` layout. The
    /// statement's, shared by its sources (granule_source/lanes.rs), derived from the part's resident `Fp128` table of the
    /// same form when there is one (`build_canon_for`: no second hash); never kept across statements.
    Canon,
    /// `Canon` with `FLAG_BPCHAR`: each entry's trimmed render's (`bcTruelen`) image.
    CanonBpchar,
    /// `table_fn::RANK` with `FLAG_BPCHAR` (OID-19): an `I64` table of each entry's rank among the part's entries without
    /// their trailing blanks (`bcTruelen`), entries equal after the trim sharing one rank (rank.rs).
    RankBpchar,
    /// `regexp_replace(text, pattern, replacement)` through the function manager (oid 2284, the 3-argument form; with
    /// `global`, oid 2285 with the flags `'g'`: every match replaced): a `Derived` table; `encoding` is the backend's
    /// database encoding, armed on the building thread (the RE2 dispatch needs UTF-8).
    RegexpReplace {
        global: bool,
        pattern: Vec<u8>, replacement: Vec<u8>, collation: u32, encoding: i32, // alloc: Program
    },
    /// The 128-bit fingerprint of `regexp_replace(text, pattern, replacement)`'s bytes per entry (the
    /// `RegexpReplace` table's values fingerprinted as `Fingerprint` does the entries): an `Fp128` table,
    /// so a derived key's identity across parts is the same class as a text key's. No fault bitmap; a [0, 0] entry means
    /// re-evaluate the row (percode_key); a consumer that needs per-entry faults declares a FINGERPRINT table instead.
    /// The statement's own: a query constant keys it, so no later statement reads it. `global`: `table_fn::FLAG_GLOBAL`.
    DerivedFingerprint {
        global: bool,
        pattern: Vec<u8>, replacement: Vec<u8>, collation: u32, encoding: i32, // alloc: Program
    },
    /// `DerivedFingerprint`'s table for any other immutable builtin of the entry and baked varlena constants (`call`,
    /// under `collation`), evaluated once per entry through the function manager: the 128-bit fingerprint of its result's
    /// bytes. An entry whose call raised or came out NULL is [0, 0] (its rows re-evaluate, percode_key). The source's own.
    /// `adaptive` (`table_fn::FLAG_ADAPTIVE`, (2')): the source may publish it unfilled instead, as a `Program` Fp128 table its
    /// rows fill (granule_source/lanes.rs `adaptive_lazy`).
    DerivedCall { call: gravity_program::table_fn::Call, collation: u32, encoding: i32, adaptive: bool },
    /// `table_fn::PROGRAM`: the source allocates an unfilled table of `elem` (a zeroed data area and fault bitmap) and the
    /// program sweeps the entries at bind; `fingerprint` is the body's, so distinct bodies over one lane are distinct tables.
    /// D3115: `swept` = `FLAG_SWEPT`: the statement's sources share one table of the part.
    Program { elem: u8, swept: bool, fingerprint: u64 },
    /// `table_fn::EQ` (B2-20): the code set of the entries whose bytes equal one of `values` (`negated`: of the others), a
    /// `Bits` table looked up in the part's byte-ordered dictionary; resident across statements like `Like`'s.
    Eq { values: Vec<Vec<u8>>, negated: bool }, // alloc: Program
    /// `table_fn::RANGE` (P5-06 / SRC-17): the entries whose bytes lie in `[from, to)` (`to` `None`: no upper bound), a
    /// `Bits` table of one code run of the part's byte-ordered dictionary; resident across statements like `Like`'s.
    Range { from: Vec<u8>, to: Option<Vec<u8>> }, // alloc: Program
    /// X5: `table_fn::EQ` with `FLAG_BOUND`: `Eq` over `values` and the text images in the site's key cells `cells`, which
    /// the program writes per execution (lowerv4 dict/percode_bind.rs `bound`); the source resolves it before it builds
    /// (granule_source `bound_spec`): `Eq`, or `Faults` when a value is NULL or not an inline image. Never built itself.
    /// P5-02: with `list` (`FLAG_LIST`) each cell holds a whole text array, every element a value.
    EqBound { values: Vec<Vec<u8>>, cells: Vec<u16>, negated: bool, list: bool }, // alloc: Program
    /// PS1-15: `table_fn::LIKE` with `FLAG_BOUND`: `Like` over the pattern `parts[0]`, the text image in key cell `cells[0]`,
    /// `parts[1]`, .. (one more part than cells), which the source assembles before it builds (granule_source
    /// `bound_spec`): `Like`, or `Faults` when a value is NULL or not an inline image. Never built itself.
    LikeBound { parts: Vec<Vec<u8>>, cells: Vec<u16>, ilike: bool, negated: bool }, // alloc: Program
    /// NEW-72 entry C: `table_fn::RANGE` with `FLAG_BOUND`: `Range` whose one end is the text image in key cell `cells[0]`
    /// (its upper end with `below`, else its lower end; that image `|| '\0'` with `nul`), which the source reads before it
    /// builds (granule_source `bound_spec`): `Range`, or `Faults` when the value is NULL or not an inline image. Never
    /// built itself.
    RangeBound { cells: Vec<u16>, below: bool, nul: bool }, // alloc: Program
    /// X5: no entry answered, every code a fault, so each row evaluates the body (a bound value the table cannot read); a
    /// strict root over a NULL bound value reads NULL without it (gravity_expr emit `bound_null`).
    Faults,
}

/// A table lane of a site: table `ordinal` over code lane `over`, its function.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TableSpec {
    pub over: u8,
    pub func: TableFn,
}

impl TableSpec {
    pub fn elem(&self) -> u8 {
        match self.func {
            TableFn::Like { .. } | TableFn::Eq { .. } | TableFn::Range { .. } | TableFn::EqBound { .. } | TableFn::LikeBound { .. } | TableFn::RangeBound { .. } | TableFn::Faults => gravity_dict::lane::ELEM_BITS,
            TableFn::OctetLength | TableFn::CharLength | TableFn::RankBpchar => gravity_dict::lane::ELEM_I64,
            TableFn::Fingerprint | TableFn::FingerprintBpchar | TableFn::Canon | TableFn::CanonBpchar | TableFn::DerivedFingerprint { .. } | TableFn::DerivedCall { .. } => gravity_dict::lane::ELEM_FP128,
            TableFn::RegexpReplace { .. } => gravity_dict::lane::ELEM_DERIVED,
            TableFn::Program { elem, .. } => elem,
        }
    }

    /// A table the statement shares across its sources (`granule_source::StmtTables`): the regexp's derived ones and
    /// textkeys step 3's canonical image table (`Canon`). A `DerivedCall` table is each source's own, built into its region
    /// chunks as the program's tables are: no buffer of its own in the reuse pool.
    pub fn shared(&self) -> bool {
        matches!(self.func, TableFn::RegexpReplace { .. } | TableFn::DerivedFingerprint { .. } | TableFn::Canon | TableFn::CanonBpchar)
    }

    /// The once-cell key's function fingerprint (the spec's hash; distinct specs never collide in practice).
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut h);
        h.finish() | 1
    }
}

impl TableFn {
    /// A build over a byte-ordered dictionary reads no entry: an equality set whose every value has byte length 0, placed
    /// by the index's length of code 0 (`build_table_inner`). Any other value's binary search reads entries.
    pub fn reads_no_entry(&self) -> bool {
        matches!(self, TableFn::Eq { values, .. } if values.iter().all(|v| v.is_empty()))
    }

    fn heap_bytes(&self) -> usize {
        let list = |l: &Vec<Vec<u8>>| l.capacity() * core::mem::size_of::<Vec<u8>>() + l.iter().map(Vec::capacity).sum::<usize>();
        let cells = |c: &Vec<u16>| c.capacity() * 2;
        match self {
            TableFn::Like { pattern, .. } => pattern.capacity(),
            TableFn::RegexpReplace { pattern, replacement, .. } | TableFn::DerivedFingerprint { pattern, replacement, .. } => pattern.capacity() + replacement.capacity(),
            TableFn::DerivedCall { call, .. } => call.args.capacity() * core::mem::size_of::<Option<Vec<u8>>>() + call.args.iter().flatten().map(Vec::capacity).sum::<usize>(),
            TableFn::Eq { values, .. } => list(values),
            TableFn::Range { from, to } => from.capacity() + to.as_ref().map_or(0, Vec::capacity),
            TableFn::EqBound { values, cells: c, .. } => list(values) + cells(c),
            TableFn::LikeBound { parts, cells: c, .. } => list(parts) + cells(c),
            TableFn::RangeBound { cells: c, .. } => cells(c),
            TableFn::OctetLength | TableFn::CharLength | TableFn::Fingerprint | TableFn::FingerprintBpchar | TableFn::Canon | TableFn::CanonBpchar | TableFn::RankBpchar | TableFn::Program { .. } | TableFn::Faults => 0,
        }
    }
}

/// A lowered program's declared table lane `i` of `site` as the `TableSpec` the source builds
/// (`gravity_program::TableDecl`, milestone-2.md §2.6 item 32); `None` for a function the hosts do not build.
pub fn spec_of(site: &gravity_program::SiteDesc, i: usize) -> Option<TableSpec> {
    use gravity_program::table_fn as f;
    let d = site.tables.get(i)?;
    let func = match d.func {
        f::FINGERPRINT if d.flags & f::FLAG_CANON != 0 && d.flags & f::FLAG_BPCHAR != 0 => TableFn::CanonBpchar,
        f::FINGERPRINT if d.flags & f::FLAG_CANON != 0 => TableFn::Canon,
        f::FINGERPRINT if d.flags & f::FLAG_BPCHAR != 0 => TableFn::FingerprintBpchar,
        f::FINGERPRINT => TableFn::Fingerprint,
        f::OCTET_LENGTH => TableFn::OctetLength,
        f::CHAR_LENGTH => TableFn::CharLength,
        f::RANK if d.flags & f::FLAG_BPCHAR != 0 => TableFn::RankBpchar,
        f::LIKE if d.flags & f::FLAG_BOUND != 0 => match (f::values(site.table_arg(d, 0))?, f::cells(site.table_arg(d, 1))?) {
            (parts, cells) if parts.len() == cells.len() + 1 => TableFn::LikeBound { parts: parts.into_iter().map(<[u8]>::to_vec).collect(), cells, ilike: d.flags & f::FLAG_ILIKE != 0, negated: d.flags & f::FLAG_NEGATED != 0 }, // alloc: Program
            _ => return None,
        },
        f::LIKE => TableFn::Like { pattern: site.table_arg(d, 0).to_vec(), ilike: d.flags & f::FLAG_ILIKE != 0, negated: d.flags & f::FLAG_NEGATED != 0 }, // alloc: Program
        f::DERIVED_FINGERPRINT => TableFn::DerivedFingerprint { pattern: site.table_arg(d, 0).to_vec(), replacement: site.table_arg(d, 1).to_vec(), collation: d.collation, encoding: d.encoding, global: d.flags & f::FLAG_GLOBAL != 0 }, // alloc: Program
        f::DERIVED_CALL => TableFn::DerivedCall { call: gravity_program::table_fn::Call::decode(site.table_arg(d, 0))?, collation: d.collation, encoding: d.encoding, adaptive: d.flags & f::FLAG_ADAPTIVE != 0 },
        f::PROGRAM => TableFn::Program { elem: d.flags & !f::FLAG_SWEPT, swept: d.flags & f::FLAG_SWEPT != 0, fingerprint: u64::from_le_bytes(site.table_arg(d, 0).try_into().ok()?) },
        f::RANGE if d.flags & f::FLAG_BOUND != 0 => match f::cells(site.table_arg(d, 1))? {
            cells if cells.len() == 1 => TableFn::RangeBound { cells, below: d.flags & f::FLAG_BELOW != 0, nul: d.flags & f::FLAG_NUL != 0 },
            _ => return None,
        },
        f::RANGE => match f::values(site.table_arg(d, 0))?.as_slice() {
            [from] => TableFn::Range { from: from.to_vec(), to: None }, // alloc: Program
            [from, to] => TableFn::Range { from: from.to_vec(), to: Some(to.to_vec()) }, // alloc: Program
            _ => return None,
        },
        f::EQ if d.flags & f::FLAG_BOUND != 0 => TableFn::EqBound { values: f::values(site.table_arg(d, 0))?.into_iter().map(<[u8]>::to_vec).collect(), cells: f::cells(site.table_arg(d, 1))?, negated: d.flags & f::FLAG_NEGATED != 0, list: d.flags & f::FLAG_LIST != 0 }, // alloc: Program
        f::EQ => TableFn::Eq { values: f::values(site.table_arg(d, 0))?.into_iter().map(<[u8]>::to_vec).collect(), negated: d.flags & f::FLAG_NEGATED != 0 }, // alloc: Program
        _ => return None,
    };
    Some(TableSpec { over: d.over, func })
}

/// A part's dictionary as the library's `Dict`: the writer's (`DictHandle`, the bulk cursor over the
/// prewarmed payload) or a verbatim part's ranks (`Ranked`: one pass over the part's rows through
/// `cur`, each code's representative row handed to the build as its entry); a read error is kept in
/// `err` for the build to raise.
pub enum DictView<'a> {
    Part(&'a DictHandle, &'a core::cell::Cell<Option<ReadError>>),
    Ranked { ranks: &'a Ranks, cur: &'a core::cell::RefCell<&'a mut StreamCursor<'static>>, scratch: &'a core::cell::RefCell<&'a mut TextScratch>, err: &'a core::cell::Cell<Option<ReadError>> },
}

impl DictView<'_> {
    /// A verbatim part's ranks (`None` for a dictionary-coded part's view).
    pub fn ranks(&self) -> Option<&Ranks> {
        match self {
            DictView::Part(..) => None,
            DictView::Ranked { ranks, .. } => Some(ranks),
        }
    }

    /// The ledger owner a build over the view charges its working memory to: a verbatim part's, its source's
    /// (`TextScratch::set_owner`); none for a dictionary-coded part's view.
    pub fn owner(&self) -> BuildOwner {
        match self {
            DictView::Part(..) => None,
            DictView::Ranked { scratch, .. } => scratch.borrow().owner,
        }
    }
}

impl gravity_dict::Dict for DictView<'_> {
    fn ncodes(&self) -> u32 {
        match self {
            DictView::Part(d, _) => d.ncodes(),
            DictView::Ranked { ranks, .. } => ranks.ncodes(),
        }
    }

    fn entries(&self, f: &mut dyn FnMut(u32, gravity_dict::View)) {
        match self {
            DictView::Part(d, err) => {
                match payload_walk(d, f) {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(e) => return err.set(Some(e)),
                }
                let mut cur = match d.entries(0, d.ncodes()) {
                    Ok(c) => c,
                    Err(e) => return err.set(Some(e)),
                };
                // D3887 (C9): an entry is one cursor step and the callee's fingerprint, tens of ns: a `batch` loop (a large
                // dictionary's walk is seconds)
                let mut step = cancelpoll::step("text_hosts: a part dictionary's entries walked");
                for n in 0usize.. {
                    if step.batch(n).is_err() {
                        return err.set(Some(ReadError::Canceled));
                    }
                    match cur.next_entry() {
                        Ok(Some((code, e))) => f(code, gravity_dict::View::of(e.bytes)),
                        Ok(None) => return,
                        Err(e) => return err.set(Some(e)),
                    }
                }
            }
            DictView::Ranked { ranks, cur, scratch, err } => {
                // D3038 (A): a table coded by length holds no entry's bytes (`lane_codes`)
                if ranks.by_length().is_some() {
                    return err.set(Some(ReadError::Format(FormatError::Corrupt { at: "D3038: the entries of a code lane coded by length" })));
                }
                let (mut cur, mut scratch) = (cur.borrow_mut(), scratch.borrow_mut());
                // D3288: each code at its first live row, by a bit per code; the test of the code's representative row was
                // a random load into a reps array past L2 (93% of a near-unique part's walk). Any live row of a code holds
                // the code's bytes, and every build sets by code, so the row a code is handed at does not matter. The bits
                // are the walk's, on an arena charged to the source's owner; a refusal is the allocator's, as a `Vec`'s.
                let a = arena_of(scratch.owner);
                let mut bits: AVec<u64> = AVec::default();
                if let Err(r) = bits.resize(&a, (ranks.ncodes() as usize).div_ceil(64), 0) {
                    std::alloc::handle_alloc_error(core::alloc::Layout::from_size_align(r.bytes as usize, 8).unwrap_or(core::alloc::Layout::new::<u64>()));
                }
                let seen = bits.as_mut_slice(&a);
                // C9: a tick per granule decoded
                let mut step = cancelpoll::step("text_hosts: a verbatim part's codes walked at their first rows");
                for lg in ranks.granules() {
                    if step.tick().is_err() {
                        return err.set(Some(ReadError::Canceled));
                    }
                    let (rows, all) = match scratch.decode(&mut cur, lg) {
                        Ok(r) => r,
                        Err(e) => return err.set(Some(e)),
                    };
                    let codes = ranks.granule_codes(lg);
                    for i in 0..rows {
                        let c = codes[i];
                        if scratch.live(all, i) && first_seen(seen, c) {
                            // a tick per code handed out: the callee (a render's copy, a fingerprint) is the unit
                            if step.tick().is_err() {
                                return err.set(Some(ReadError::Canceled));
                            }
                            f(c, gravity_dict::View::of(raw::payload(scratch.datums[i])));
                        }
                    }
                }
            }
        }
    }
}

/// The renders of one code lane over its source's current part. A hosted fold (class.text:
/// min / max under a collation that does not order by bytes) renders every row through `dict_value`, an emit a few
/// winners; past `RENDER_MEMO_AFTER` renders in a row on a dictionary-coded part the part's renders are memoised per code,
/// and on a verbatim part (`Ranks`), whose render decodes its code's representative row alone, `RENDER_WALK_AFTER`
/// renders take the part's render table (`render_table`: every code's image, resident and shared by the participants and
/// statements that render the part) with no per-code memo before it (a verbatim part's codes are about its rows, so a
/// memo of them per participant and statement was as large as the part's codes). A new part (or generation) starts
/// over, so winners rendered across parts arm nothing; but a verbatim part's count and table are parked at a switch and
/// taken back when the renders return to it (D3324 R1: a build's payload codes arrive in probe order, alternating the
/// build's parts, so a count kept per switch never reached `RENDER_WALK_AFTER`; `PGRUST_GRAVITY_RENDER_PARK=0` starts
/// every switch over, as before). The
/// render tables taken stay held for the statement (a fold or an emit may hold an image past its part): the source drops
/// the memo at unbind.
pub struct RenderMemo {
    /// The memo's capacity on its owner (`set_owner`), synced when the memo is armed; dropped before the memo's buffer.
    charge: Option<memledger::Charge>,
    key: Option<(usize, u64, u32)>,
    renders: u32,
    /// The current part's render table, once its renders earned it.
    table: Option<Arc<FpTable>>,
    /// Whether the current part is verbatim (its count and table park at a switch).
    verbatim: bool,
    /// The earlier parts' render tables, held for the statement.
    held: Vec<Arc<FpTable>>, // alloc: Node
    /// D3324 R1: the verbatim parts the renders left, by part index: each its generation, count and table (moved, never
    /// cloned: a clone per switch put every participant on the shared table's refcount line).
    parked: Vec<Parked>, // alloc: Node
    owner: BuildOwner,
    memo: Vec<(u64, u32)>, // alloc: Node charged self.owner
}

/// A verbatim part's renders parked at a switch (`RenderMemo::parked`): the part's `(epoch, opened)`, its count and its
/// render table once earned.
#[derive(Default)]
struct Parked {
    gen: (u64, u32),
    renders: u32,
    table: Option<Arc<FpTable>>,
}

const RENDER_MEMO_AFTER: u32 = 64;
const RENDER_WALK_AFTER: u32 = 256;

impl RenderMemo {
    pub const fn new() -> RenderMemo {
        RenderMemo { key: None, renders: 0, table: None, verbatim: false, held: Vec::new(), parked: Vec::new(), charge: None, owner: None, memo: Vec::new() } // alloc: Node
    }

    /// The owner the memo's bytes are charged to from now on (the source's `BuildOwner`): a new owner takes the memo's
    /// capacity over (the old owner's charge ends first).
    pub fn set_owner(&mut self, owner: BuildOwner) {
        if owner != self.owner {
            self.owner = owner;
            self.charge = None;
            self.sync_charge();
        }
    }

    pub fn owner(&self) -> BuildOwner {
        self.owner
    }

    /// The memo's charge at its capacity, opened on the owner at its first bytes.
    fn sync_charge(&mut self) {
        use core::mem::size_of;
        let cap = (self.memo.capacity() * size_of::<(u64, u32)>() + self.parked.capacity() * size_of::<Parked>() + self.held.capacity() * size_of::<Arc<FpTable>>()) as u64;
        if self.charge.is_none() && cap > 0 {
            self.charge = table_charge(self.owner, 0);
        }
        charge_to(&mut self.charge, cap);
    }

    /// D3213: the lists a parked source keeps (`Source::kept_bytes`).
    pub fn kept_bytes(&self) -> usize {
        gravity_driver::vec_bytes(&self.held) + gravity_driver::vec_bytes(&self.parked) + gravity_driver::vec_bytes(&self.memo)
    }

    /// The memoised `(image, byte_len)` of `code` in part `p` (generation `epoch`, `ncodes` codes), `None` when the part
    /// has none for it yet; counts the render. A verbatim part (`verbatim`) keeps no per-code memo: its renders read its
    /// render table once `walk_due` took it, and decode their rows before. D4327: `opened` is the statement's release count
    /// of the part (`StmtDicts::epoch`, 0 with none): a dictionary the statement released and opened again keeps the part's
    /// `epoch` (minted once per open part) but not its image, so the memo's pointers into the released one start over.
    #[inline]
    pub fn get(&mut self, p: usize, epoch: u64, opened: u32, ncodes: u32, code: u32, verbatim: bool) -> Option<(u64, u32)> {
        if self.key != Some((p, epoch, opened)) {
            self.switch(p, epoch, opened, verbatim);
        }
        self.renders = self.renders.saturating_add(1);
        if let Some(t) = &self.table {
            return t.render(code);
        }
        if verbatim {
            return None;
        }
        if self.memo.is_empty() {
            if self.renders < RENDER_MEMO_AFTER || ncodes == 0 {
                return None;
            }
            self.memo.resize(ncodes as usize, (0, 0)); // alloc: Node charged self.owner
            self.sync_charge();
        }
        self.memo.get(code as usize).copied().filter(|e| e.0 != 0)
    }

    /// The renders move to part `p` (generation `(epoch, opened)`): the dictionary memo starts over; a verbatim part's
    /// count and table park by part index and come back when the renders return to that part's generation (D3324 R1). A
    /// table parked under an older generation of the part stays held for the statement, as every left table does with
    /// `PGRUST_GRAVITY_RENDER_PARK=0`.
    #[cold]
    #[inline(never)]
    fn switch(&mut self, p: usize, epoch: u64, opened: u32, verbatim: bool) {
        let park = crate::inputs::render_park_enabled();
        let (renders, table) = (core::mem::take(&mut self.renders), self.table.take());
        match self.key {
            Some((q, e, o)) if park && self.verbatim => {
                if self.parked.len() <= q {
                    self.parked.resize_with(q + 1, Parked::default); // alloc: Node charged self.owner
                }
                let slot = &mut self.parked[q];
                if let Some(t) = slot.table.take() {
                    self.held.push(t); // alloc: Node charged self.owner
                }
                *slot = Parked { gen: (e, o), renders, table };
            }
            _ => {
                if let Some(t) = table {
                    self.held.push(t); // alloc: Node charged self.owner
                }
            }
        }
        self.key = Some((p, epoch, opened));
        self.verbatim = verbatim;
        self.memo.clear();
        if let Some(slot) = self.parked.get_mut(p).filter(|_| park && verbatim) {
            if slot.gen == (epoch, opened) {
                self.renders = core::mem::take(&mut slot.renders);
                self.table = slot.table.take();
                if self.table.is_some() {
                    fires::fire_shared("dict.render_parked"); // shapecheck fire signal (D3324 R1): a verbatim part's render table came back at a switch
                }
            } else if let Some(t) = slot.table.take() {
                self.held.push(t); // alloc: Node charged self.owner
            }
        }
        self.sync_charge();
    }

    /// `code`'s render kept for the rest of the part (once the memo is armed).
    #[inline]
    pub fn put(&mut self, code: u32, hit: (u64, u32)) {
        if let Some(e) = self.memo.get_mut(code as usize) {
            *e = hit;
        }
    }

    /// A verbatim part's renders have earned its render table.
    pub fn walk_due(&self) -> bool {
        self.table.is_none() && self.renders >= RENDER_WALK_AFTER
    }

    /// The current part's render table (`render_table`), read by the part's later renders.
    pub fn take_table(&mut self, table: Arc<FpTable>) {
        self.table = Some(table);
    }
}

/// The payload view: a part whose payload is one resident slice (the whole arm; the block arm
/// after its prewarm, which a walk of every entry faults anyway) hands each entry to `f` straight off its index record:
/// the `byte_len` bytes four past `payload_off`, a subslice of `payload_bytes` (`Derived::build_over`'s `over`). No frame
/// refill and no char length (`f` takes bytes alone; the absent form's UTF-8 count was the cursor's per-entry cost), but
/// every check the bulk cursor makes, each with its error: the 4B-U header of that length, the payload's bounds and a
/// delta char field within the length. Counted where the cursor counts its entries. `false`: a frame-served or
/// front-coded arm, which the bulk cursor walks.
fn payload_walk(d: &DictHandle, f: &mut dyn FnMut(u32, gravity_dict::View)) -> Result<bool, ReadError> {
    let Some(payload) = d.payload_bytes()? else { return Ok(false) };
    let index = d.index_bytes()?;
    let delta = matches!(d.charlen_form(), pgrc_format::dict::DictCharLenForm::Delta);
    // D3887 (C9): an entry is a checked index read and the callee's fingerprint, tens of ns: a `batch` loop
    let mut step = cancelpoll::step("text_hosts payload_walk: a resident payload's entries walked");
    for (code, rec) in index.chunks_exact(INDEX_ENTRY).enumerate() {
        step.batch(code)?;
        let off = u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]) as usize;
        let len = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]);
        let header = payload.get(off..off + 4).map(|h| u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as u64);
        if header != Some((len as u64 + 4) << 2) {
            // the cursor's error for this entry: its header's (varlena_entry_at's), else the index's length disagrees
            pgrc_format::wire::varlena_entry_at(payload, off, "dict payload").map_err(ReadError::Format)?;
            return Err(ReadError::Format(FormatError::Corrupt { at: "dict entry byte_len" }));
        }
        let Some(bytes) = payload.get(off + 4..off + 4 + len as usize) else { return Err(ReadError::Format(FormatError::Bounds { at: "dict payload" })) };
        if delta && u32::from_le_bytes([rec[8], rec[9], rec[10], rec[11]]) > len {
            return Err(ReadError::Format(FormatError::Corrupt { at: "dict entry char_len delta" }));
        }
        f(code as u32, gravity_dict::View::of(bytes));
    }
    // the entries a walk handed on, in the census the bulk cursor counts them in (the REPORT's dictent walked)
    pgrc_read::dicthandle::DICT_CURSOR_ENTRIES.fetch_add((index.len() / INDEX_ENTRY) as u64, Ordering::Relaxed);
    Ok(true)
}

/// The in-place derived table's build over a resident payload (V4R-30, L-DERIVE): `payload_walk`'s checked index walk
/// and the regexp memo pulled by `Fp128::build_derived_seq` in one loop (no per-entry dyn call); a memo hit whose prefix
/// was the previous subject's reuses its fingerprint unread. The same table as `build_derived_over`'s sweep. `None`: the
/// program refused an entry (its step budget), the region given back: the caller builds through the faulting path.
fn derived_seq(dict: &DictView<'_>, d: &DictHandle, region: &mut gravity_dict::Region, payload: &[u8], prog: &gravity_dict::regexp::Program, tmpl: &gravity_dict::regexp::Template, rep: &[u8]) -> Result<Option<gravity_dict::Fp128>, TableBuildError> {
    let index = d.index_bytes().map_err(TableBuildError::Read)?;
    let delta = matches!(d.charlen_form(), pgrc_format::dict::DictCharLenForm::Delta);
    let (mut memo, mut code, mut last_set, mut failed, mut stopped, mut refused) = (gravity_dict::regexp::Memo::new(), 0usize, false, None, false, false);
    let mark = region.mark();
    // a scratch region for the plan's non-in-place fallback (never taken by an in-place plan's single group)
    let mut scratch = vec![0u64; 1024]; // alloc: Cold
    let mut sr = raw::region(&mut scratch);
    // D3887 (C9): an entry is one regexp memo step, tens of ns on a hit: a `batch` loop (the sequence ends; the build is
    // dropped below)
    let mut step = cancelpoll::step("text_hosts derived_seq: a derived table's entries built");
    let t = gravity_dict::Fp128::build_derived_seq(dict, region, payload, || {
        if step.batch(code).is_err() {
            stopped = true;
            return None;
        }
        let rec = index.get(code * INDEX_ENTRY..(code + 1) * INDEX_ENTRY)?;
        let c = code as u32;
        code += 1;
        let off = u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]) as usize;
        let len = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]);
        let header = payload.get(off..off + 4).map(|h| u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as u64);
        let bytes = match (header == Some((len as u64 + 4) << 2), payload.get(off + 4..off + 4 + len as usize)) {
            (true, Some(b)) if !(delta && u32::from_le_bytes([rec[8], rec[9], rec[10], rec[11]]) > len) => b,
            _ => {
                failed = Some(code - 1);
                return None;
            }
        };
        let (hits, sets) = (memo.hits, memo.sets);
        let v = match gravity_dict::regexp::replace_first_memo(prog, tmpl, rep, bytes, &mut sr, &mut memo) {
            Ok(Some(v)) => v,
            Ok(None) => {
                refused = true;
                return None;
            }
            Err(e) => return Some(Err(e)),
        };
        let same = memo.hits != hits && last_set;
        last_set = memo.hits != hits || memo.sets != sets;
        // the value is a slice of the entry's bytes (the in-place plan), live through the build
        Some(Ok((c, raw::view_bytes(v), same)))
    });
    if stopped {
        return Err(TableBuildError::Read(ReadError::Canceled));
    }
    if failed.is_some() {
        // the checked walk's error for this entry (payload_walk's): re-walk to raise it
        return match payload_walk(d, &mut |_, _| {}) {
            Err(e) => Err(TableBuildError::Read(e)),
            Ok(_) => Err(TableBuildError::Read(ReadError::Format(FormatError::Corrupt { at: "dict entry" }))),
        };
    }
    if refused {
        region.reset_to(mark);
        return Ok(None);
    }
    let t = t?;
    SCANNED_ENTRIES.fetch_add(code as u64, Ordering::Relaxed);
    pgrc_read::dicthandle::DICT_CURSOR_ENTRIES.fetch_add(code as u64, Ordering::Relaxed);
    Ok(Some(t))
}

/// The fingerprint seed, installed once (`gravity_dict::fp::install`): 16 bytes of OS entropy (`pg_strong_random`, the
/// sanctioned entropy funnel: seeded under DST) drawn once per process and never written anywhere, so an adversary who
/// learns one process's fingerprints cannot aim collisions at the next one. Every table keyed by it is a process cache.
/// `PGRUST_GRAVITY_FP_SEED=<32 hex>` pins it, for tests and DST only.
pub fn fp_seed() -> (u64, u64) {
    *SEED.get_or_init(|| {
        let mut kb = [0u8; 16];
        match gravity_driver::env::text("PGRUST_GRAVITY_FP_SEED") {
            Some(s) if s.trim().len() == 32 && s.trim().bytes().all(|c| c.is_ascii_hexdigit()) => {
                // bounded("small-fixed: the seed's 16 bytes")
                for (i, b) in kb.iter_mut().enumerate() {
                    *b = u8::from_str_radix(&s.trim()[2 * i..2 * i + 2], 16).expect("hex");
                }
            }
            _ => assert!(pg_strong_random::pg_strong_random(&mut kb), "gravity: OS entropy unavailable for the fingerprint seed"),
        }
        gravity_dict::fp::install(u64::from_le_bytes(kb[..8].try_into().expect("8")), u64::from_le_bytes(kb[8..].try_into().expect("8")));
        gravity_dict::fp::seed().expect("installed")
    })
}

static SEED: OnceLock<(u64, u64)> = OnceLock::new();

/// The boot draw (shared-tables.md §2.1) once the data directory is known (a server, not a rig); else the first builder draws.
pub fn fp_seed_boot() {
    if init_small::globals::DataDir().is_some() {
        fp_seed();
    }
}

/// `length(text)` under UTF-8: the bytes that start a character (eight bytes a step over the ASCII body).
pub fn utf8_chars(b: &[u8]) -> i64 {
    let (mut n, mut rest) = (0usize, b);
    // bounded("other: one value's bytes eight at a time, a varlena of at most 1 GB (about 0.1 s at that size); PG's length() counts a value's characters with no poll inside")
    while rest.len() >= 8 {
        let w = u64::from_le_bytes(rest[..8].try_into().expect("8"));
        n += 8 - ((w >> 7) & !(w >> 6) & 0x0101_0101_0101_0101).count_ones() as usize;
        rest = &rest[8..];
    }
    (n + rest.iter().filter(|&&c| c & 0xC0 != 0x80).count()) as i64
}

const REGEXP_REPLACE_OID: types_core::Oid = 2284;
/// `regexp_replace(text, text, text, text)`: the flags form.
const REGEXP_REPLACE_FLAGS_OID: types_core::Oid = 2285;
const PG_UTF8: i32 = 6;
const INDEX_ENTRY: usize = pgrc_format::dict::DICT_INDEX_ENTRY_LEN;

/// A part's dictionary index as the library's `EntryIndex`: entry `code` is the 4B-U image at
/// `payload_off`, its bytes four past it, `byte_len` long.
struct PgrcIndex<'a> {
    index: &'a [u8],
}

impl PgrcIndex<'_> {
    #[inline(always)]
    fn field(&self, code: u32, k: usize) -> usize {
        let at = code as usize * INDEX_ENTRY + 4 * k;
        u32::from_le_bytes(self.index[at..at + 4].try_into().expect("4")) as usize
    }
}

impl gravity_dict::EntryIndex for PgrcIndex<'_> {
    fn ncodes(&self) -> u32 {
        (self.index.len() / INDEX_ENTRY) as u32
    }

    #[inline(always)]
    fn span(&self, code: u32) -> (usize, usize) {
        (self.field(code, 0) + 4, self.field(code, 1))
    }
}

/// How a `RegexpReplace` spec is evaluated: the pattern program per entry (`gravity_dict::regexp`,
/// UTF-8 only; a `(?i)` pattern folded under a collation whose regex case map is ASCII's,
/// `gravity_fn::typesem::collation::regex_ascii_cases`) or the function manager behind the required-literal prefilter,
/// the engine's automaton per entry where the call would run it (`automaton`).
pub enum RegexpPlan {
    Program { prog: gravity_dict::regexp::Program, tmpl: gravity_dict::regexp::Template, in_place: bool },
    Fmgr { required: Option<(usize, usize)> },
}

impl RegexpPlan {
    pub fn of(pattern: &[u8], replacement: &[u8], collation: u32, encoding: i32) -> RegexpPlan {
        if encoding == PG_UTF8 {
            let prog = gravity_dict::regexp::compile_under(pattern, || gravity_fn::typesem::collation::regex_ascii_cases(collation));
            if let (Some(prog), Some(tmpl)) = (prog, gravity_dict::regexp::Template::parse(replacement)) {
                let in_place = tmpl.single_group() && prog.anchored() && prog.anchored_to_end();
                return RegexpPlan::Program { prog, tmpl, in_place };
            }
        }
        RegexpPlan::Fmgr { required: gravity_dict::regexp::required_literal(pattern) }
    }

    pub fn open_coded(&self) -> bool {
        matches!(self, RegexpPlan::Program { .. })
    }
}

/// B2-09: the automaton the 3-argument `regexp_replace` call (`REG_ADVANCED`) runs for `pattern` through the engine's own
/// dispatch (RE2 under regexp_alt's fail-closed classifier, `replace_text_regexp`'s first arm), which every entry the
/// engine routes there (`Re2Pattern::subject_ok`, the pattern's own data guard) shares; `None` keeps the call (Spencer, a capture RE2 cannot place for
/// the replacement, another encoding, a pattern the engine refused).
pub fn automaton(pattern: &[u8], replacement: &[u8], encoding: i32) -> Option<regexp_alt::Re2Pattern> {
    if encoding != PG_UTF8 {
        return None;
    }
    let re = regexp_alt::dispatch(pattern, regex_spencer::REG_ADVANCED, b"").ok()??;
    (re.capture_safe() || regexp_alt::check_replace_text_has_escape(replacement) < 2).then_some(re)
}

/// The statement's `regexp_replace` call under `collation`: the bound one, rebound when a spec of another collation or
/// form bound it. `global`: the 4-argument form (oid 2285), called with the flags `'g'` (`RegexpImages`).
fn regexp_call(fmgr: &mut Option<gravity_fn::FmgrCall>, collation: u32, global: bool) -> Result<&mut gravity_fn::FmgrCall, TableBuildError> {
    let (oid, nargs) = regexp_fn(global);
    if fmgr.as_ref().is_some_and(|c| c.collation() != collation || c.nargs() != nargs) {
        *fmgr = None;
    }
    match fmgr {
        Some(c) => Ok(c),
        None => Ok(fmgr.insert(gravity_fn::FmgrCall::bind(oid, nargs, collation).map_err(TableBuildError::Pg)?)),
    }
}

/// `regexp_replace`'s function and arity: the 3-argument form (the first match), or with `global` the 4-argument one.
fn regexp_fn(global: bool) -> (types_core::Oid, usize) {
    if global { (REGEXP_REPLACE_FLAGS_OID, 4) } else { (REGEXP_REPLACE_OID, 3) }
}

/// A `regexp_replace` call's constant images: the pattern, the replacement and, for the 4-argument form, the flags `'g'`.
struct RegexpImages {
    pat: Vec<u8>,
    rep: Vec<u8>,
    flags: Option<Vec<u8>>,
}

impl RegexpImages {
    fn new(pattern: &[u8], replacement: &[u8], global: bool) -> RegexpImages {
        RegexpImages { pat: text_image(pattern), rep: text_image(replacement), flags: global.then(|| text_image(b"g")) } // alloc: Cold
    }

    /// The call over the subject image `arg`.
    fn call(&self, call: &mut gravity_fn::FmgrCall, arg: &[u8]) -> PgResult<datum::NullableDatum> {
        let d = |b: &[u8]| datum::Datum::from_usize(b.as_ptr() as usize);
        match &self.flags {
            Some(f) => call.call_site(&[d(arg), d(&self.pat), d(&self.rep), d(f)], &[false; 4]),
            None => call.call_site(&[d(arg), d(&self.pat), d(&self.rep)], &[false; 3]),
        }
    }
}

/// The SQLSTATEs PostgreSQL's `regexp_replace` raises while it compiles, before it reads the subject.
const REGEXP_COMPILE_ERRORS: [types_error::SqlState; 4] = [
    types_error::ERRCODE_INVALID_REGULAR_EXPRESSION,
    types_error::ERRCODE_INVALID_PARAMETER_VALUE,
    types_error::ERRCODE_FEATURE_NOT_SUPPORTED,
    types_error::ERRCODE_INDETERMINATE_COLLATION,
];

/// D354: PostgreSQL compiles the pattern under the call's flags and collation before any row, so a compile error (2201B a
/// bad pattern, 22023 bad flags, 0A000 a nondeterministic collation, 42P22 none) is every evaluated row's. One call over
/// an empty subject through the rows' own function and collation: `true` when it raised one, and the build then faults
/// every entry without trusting the prefilter or the open-coded program. Any other error (a cancel) aborts the build.
fn regexp_compile_fails(pattern: &[u8], replacement: &[u8], collation: u32, encoding: i32, global: bool, fmgr: &mut Option<gravity_fn::FmgrCall>) -> Result<bool, TableBuildError> {
    gravity_fn::typesem::collation::arm_database_encoding(encoding).map_err(TableBuildError::Pg)?;
    let call = regexp_call(fmgr, collation, global)?;
    let r = RegexpImages::new(pattern, replacement, global).call(call, &text_image(b"")); // alloc: Cold
    call.reset_window();
    match r {
        Ok(_) => Ok(false),
        Err(e) if REGEXP_COMPILE_ERRORS.contains(&e.sqlstate()) => Ok(true),
        Err(e) => Err(TableBuildError::Pg(e)),
    }
}

/// Build the table `spec` over `dict` into `region`: the table header (published by the caller).
/// `RegionFull` is the caller's retry after growing the region.
pub fn build_table(spec: &TableSpec, dict: &DictView<'_>, region: &mut gravity_dict::Region, fmgr: &mut Option<gravity_fn::FmgrCall>) -> Result<*const gravity_dict::table::Header, TableBuildError> {
    build_table_for(spec, dict, region, fmgr, None)
}

/// `build_table` with its working memory charged to `owner` (a verbatim part's view: its source's when `owner` is none).
pub fn build_table_for(spec: &TableSpec, dict: &DictView<'_>, region: &mut gravity_dict::Region, fmgr: &mut Option<gravity_fn::FmgrCall>, owner: BuildOwner) -> Result<*const gravity_dict::table::Header, TableBuildError> {
    stack_depth_core::check_stack_depth().map_err(TableBuildError::Pg)?;
    let hdr = build_table_inner(spec, dict, region, fmgr, owner.or(dict.owner()))?;
    let err = match dict {
        DictView::Part(_, err) | DictView::Ranked { err, .. } => err,
    };
    if let Some(e) = err.take() {
        return Err(TableBuildError::Read(e));
    }
    Ok(hdr)
}

/// A part dictionary's index and (after the prewarm) its contiguous payload; `None` on a frame-served arm.
fn part_view<'a>(d: &'a DictHandle) -> Result<Option<(PgrcIndex<'a>, &'a [u8])>, TableBuildError> {
    d.prewarm_payload().map_err(TableBuildError::Read)?;
    let Some(payload) = d.payload_bytes().map_err(TableBuildError::Read)? else { return Ok(None) };
    Ok(Some((PgrcIndex { index: d.index_bytes().map_err(TableBuildError::Read)? }, payload)))
}

/// `v` without its trailing blanks (`bcTruelen`, varchar.c:670): a bpchar entry's equality bytes.
fn bpchar_trimmed(v: gravity_dict::View) -> gravity_dict::View {
    let b = raw::view_bytes(v);
    gravity_dict::View { ptr: v.ptr, len: b.iter().rposition(|&c| c != b' ').map_or(0, |i| i + 1) as u32 }
}

/// The codes `[lo, hi)` of the entries equal to `lit` in a byte-ordered dictionary (LIKE without a wildcard): from
/// `lit`'s lower bound to `lit || '\0'`'s, as no byte string lies between the two.
/// `owner`: the build's, whose the search key's copy is.
fn exact_run<'a, E>(n: u32, lit: &[u8], entry: &mut dyn FnMut(u32) -> Result<&'a [u8], E>, owner: BuildOwner) -> Result<(u32, u32), E> {
    let lo = gravity_dict::lookup::lower_bound(n, lit, entry)?;
    let mut past = memledger::ChargedVec::<u8>::new(owner.map(|o| o.0), owner.map_or(memledger::Kind::Region, |o| o.1));
    past.with_vec(|v| {
        v.reserve_exact(lit.len() + 1); // alloc: Exec charged owner
        v.extend_from_slice(lit); // alloc: Exec charged owner
        v.push(0); // alloc: Exec charged owner
    });
    Ok((lo, gravity_dict::lookup::lower_bound(n, &past, entry)?))
}

fn build_table_inner(spec: &TableSpec, dict: &DictView<'_>, region: &mut gravity_dict::Region, fmgr: &mut Option<gravity_fn::FmgrCall>, owner: BuildOwner) -> Result<*const gravity_dict::table::Header, TableBuildError> {
    use gravity_dict::{Codes, Derived, Fp128, I64};
    // D3038 (A): a table coded by length (`lane_codes`) answers functions of the length alone (exec sites.rs `lens_only`):
    // its codes rank lengths ascending, so '' (the one value of length 0) is code 0 where the part holds it
    if let DictView::Ranked { ranks, .. } = dict {
        if let Some(lens) = ranks.by_length() {
            return match &spec.func {
                TableFn::OctetLength => Ok(I64::build_by_code(&Codes { ncodes: ranks.ncodes(), null: None }, region, &mut |c| Ok(lens[c as usize]))?.header()),
                TableFn::Eq { values, negated } if !values.is_empty() && values.iter().all(|v| v.is_empty()) => {
                    let codes: &[u32] = if lens.first() == Some(&0) { &[0] } else { &[] };
                    Ok(gravity_dict::Bits::of_codes(dict, codes, *negated, region)?.header())
                }
                _ => Err(TableBuildError::Pg(Box::new(types_error::PgError::error("gravity: Reason::Bug: D3038: a table other than a function of the length over a code lane coded by length").with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR)))), // alloc: Cold
            };
        }
    }
    match &spec.func {
        TableFn::Like { pattern, ilike, negated } => {
            let mode = if *ilike { gravity_dict::like::Mode::ILike } else { gravity_dict::like::Mode::Like };
            let mut m = gravity_dict::like::Matcher::new(pattern, mode, region)?;
            if *negated {
                m = m.negated();
            }
            // B2-01: the contains scan's rare byte pair re-ranked from the dictionary's own bytes when its projected waste pays for the count
            m = m.recounting(|scanned, rejected, counted, left| gravity_program::cost::scan::Scan::REFERENCE.recount_pays(scanned, rejected, counted, left));
            if m.class() == gravity_dict::like::Class::Segments {
                fires::fire_shared("like.segments"); // shapecheck fire signal (D3288): runs between %s, each by its own scan
            }
            if let DictView::Part(d, _) = dict {
                // P5-06: over a part's byte-ordered codes, LIKE 'c%' (and 'c', and '%') is one code run, its ends by binary
                // search (a few entries read, never the payload): the entries from c to c's byte successor (`prefix_run`)
                let n = d.ncodes();
                let run = match (*ilike || !d.byte_rank_sorted(), m.class()) {
                    (false, gravity_dict::like::Class::Prefix) => Some(gravity_dict::lookup::prefix_run(n, m.literal(), &mut |c| d.entry(c).map(|e| e.bytes)).map_err(TableBuildError::Read)?),
                    (false, gravity_dict::like::Class::Exact) => Some(exact_run(n, m.literal(), &mut |c| d.entry(c).map(|e| e.bytes), owner).map_err(TableBuildError::Read)?),
                    (false, gravity_dict::like::Class::Any) => Some((0, n)),
                    _ => None,
                };
                if let Some((lo, hi)) = run {
                    return Ok(gravity_dict::Bits::of_run(dict, lo, hi, *negated, region)?.header());
                }
                if let Some((idx, payload)) = part_view(d)? {
                    return Ok(m.build_bits_over(&idx, payload, region)?.header());
                }
            }
            if let DictView::Ranked { ranks, cur, scratch, .. } = dict {
                // D3288 (C): a verbatim part's streaming pattern (a contains literal, % runs) scans each decoded granule
                // once, the hits resolved to rows and their codes, never a call per entry
                if m.streams() {
                    return Ok(ranked_scan_bits(&m, dict, ranks, cur, scratch, region)?.header());
                }
            }
            Ok(m.build_bits(dict, region)?.header())
        }
        TableFn::OctetLength => {
            // a part's byte lengths ARE the dictionary index's `byte_len` field: the lane points into the resident index (no table, R4)
            if let DictView::Part(d, _) = dict {
                let index = d.index_bytes().map_err(TableBuildError::Read)?;
                return Ok(raw::index_len_header(region, index, gravity_dict::LenField::Bytes)?);
            }
            // D3038: a verbatim part's lengths from its entries' headers alone (no full decode of the representative rows)
            if let DictView::Ranked { ranks, cur, scratch, .. } = dict {
                let (mut cur, mut scratch) = (cur.borrow_mut(), scratch.borrow_mut());
                // one word per code, on an arena charged to the source's owner for the build
                let a = arena_of(scratch.owner);
                let mut lens: AVec<i64> = AVec::default();
                lens.resize(&a, ranks.ncodes() as usize, 0).map_err(refused)?;
                scratch.ranked_lengths(&mut cur, ranks, lens.as_mut_slice(&a)).map_err(TableBuildError::Read)?;
                let lens = lens.as_slice(&a);
                return Ok(I64::build_by_code(&Codes { ncodes: ranks.ncodes(), null: None }, region, &mut |c| Ok(lens[c as usize]))?.header());
            }
            Ok(I64::build(dict, region, &mut |v| Ok(raw::view_bytes(v).len() as i64))?.header())
        }
        TableFn::RankBpchar => Ok(rank::build(dict, owner, region)?.header()),
        TableFn::CharLength => {
            if let DictView::Part(d, _) = dict {
                // the stored char fact (`DictHandle::lengths`, the stored-lengths law; what the SQE oracle reads too): the index's
                // char field, absolute or as `byte_len − char_field`; only the absent form (nothing stored) is swept per statement
                use pgrc_format::dict::DictCharLenForm;
                let field = match d.charlen_form() {
                    DictCharLenForm::Absolute => Some(gravity_dict::LenField::CharsAbsolute),
                    DictCharLenForm::Delta => Some(gravity_dict::LenField::CharsDelta),
                    DictCharLenForm::Absent => None,
                };
                if let Some(field) = field {
                    let index = d.index_bytes().map_err(TableBuildError::Read)?;
                    return Ok(raw::index_len_header(region, index, field)?);
                }
                if let Some((idx, payload)) = part_view(d)? {
                    let pd = gravity_dict::PayloadDict { index: &idx, payload };
                    return Ok(I64::build(&pd, region, &mut |v| Ok(utf8_chars(raw::view_bytes(v))))?.header());
                }
            }
            Ok(I64::build(dict, region, &mut |v| Ok(utf8_chars(raw::view_bytes(v))))?.header())
        }
        TableFn::Fingerprint | TableFn::FingerprintBpchar => {
            fp_seed();
            if let DictView::Part(d, _) = dict {
                d.prewarm_payload().map_err(TableBuildError::Read)?;
            }
            let trim = spec.func == TableFn::FingerprintBpchar;
            Ok(Fp128::build(dict, region, &mut |v| Ok(if trim { bpchar_trimmed(v) } else { v }))?.header())
        }
        TableFn::Canon | TableFn::CanonBpchar => {
            // textkeys step 3: a source's own build (granule_source/lanes.rs shares one per statement instead): each long
            // entry hashed here, as its Fp128 table's build would
            fp_seed();
            if let DictView::Part(d, _) = dict {
                d.prewarm_payload().map_err(TableBuildError::Read)?;
            }
            let trim = matches!(spec.func, TableFn::CanonBpchar);
            Ok(Fp128::build_canon(dict, region, &mut |v| Ok(if trim { bpchar_trimmed(v) } else { v }), None)?.header())
        }
        TableFn::DerivedFingerprint { pattern, replacement, collation, encoding, global } => {
            fp_seed();
            // lowerv4's Fp128 convention (percode_bind.rs sweep): a faulted entry (an input the regexp raised 2201B on) stays
            // [0, 0], which sends its rows to the re-evaluation, and the table carries no fault bitmap (agg_bind binds no
            // BIND_FP table with one: V4R-17); every other error aborts the build, so a fault here is only that.
            let no_faults = raw::no_faults;
            // the in-place plan over a resident payload: each derived value a subslice of the payload,
            // fingerprinted in the sweep (Fp128::build_derived_over); statement-scoped like every table here
            if let DictView::Part(d, _) = dict {
                if !regexp_compile_fails(pattern, replacement, *collation, *encoding, *global, fmgr)? {
                    // in place: anchored at both ends, so `'g'`'s one match is the first
                    if let RegexpPlan::Program { prog, tmpl, in_place: true } = RegexpPlan::of(pattern, replacement, *collation, *encoding) {
                        if let Some(p) = d.payload_bytes().map_err(TableBuildError::Read)? {
                            if let Some(t) = derived_seq(dict, d, region, p, &prog, &tmpl, replacement)? {
                                return Ok(no_faults(t.header() as *mut gravity_dict::table::Header));
                            }
                        }
                    }
                }
            }
            // otherwise the derived values built once into a buffer of their own (freed here), fingerprinted by code
            let inner = TableSpec { over: spec.over, func: TableFn::RegexpReplace { pattern: pattern.clone(), replacement: replacement.clone(), collation: *collation, encoding: *encoding, global: *global } }; // alloc: Exec
            let t = build_shared_for(&inner, dict, fmgr, owner)?;
            let d = raw::derived_at(t.header());
            let codes = Codes { ncodes: d.ncodes(), null: gravity_dict::Dict::null_code(dict) };
            // a faulting code's canonical is `u32::MAX` (the fault bit carries over), the rest fingerprint once per distinct value
            let hdr = Fp128::build_by_canon(&codes, region, d.ndistinct(), &mut |c| raw::derived_canonical(&d, c), &mut |c| raw::derived_view(&d, c))?.header();
            Ok(no_faults(hdr as *mut gravity_dict::table::Header))
        }
        TableFn::DerivedCall { call, collation, encoding, .. } => {
            fp_seed();
            // one sweep: each entry's result fingerprinted straight into the table; a faulted code is [0, 0] with no fault
            // bitmap (DerivedFingerprint's convention)
            let hdr = call_fingerprints(call, *collation, *encoding, dict, region)?;
            Ok(raw::no_faults(hdr as *mut gravity_dict::table::Header))
        }
        TableFn::Program { elem, .. } => {
            if let DictView::Part(d, _) = dict {
                d.prewarm_payload().map_err(TableBuildError::Read)?;
            }
            empty_table(dict, region, *elem)
        }
        TableFn::Faults => Ok(gravity_dict::Bits::all_faults(dict, region)?.header()),
        TableFn::EqBound { .. } => Err(TableBuildError::Pg(Box::new(types_error::PgError::error("gravity: Reason::Bug: a bound code set built before its values were read").with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR)))), // alloc: Cold
        TableFn::LikeBound { .. } => Err(TableBuildError::Pg(Box::new(types_error::PgError::error("gravity: Reason::Bug: a bound LIKE table built before its pattern was read").with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR)))), // alloc: Cold
        TableFn::RangeBound { .. } => Err(TableBuildError::Pg(Box::new(types_error::PgError::error("gravity: Reason::Bug: a bound byte interval built before its end was read").with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR)))), // alloc: Cold
        TableFn::Eq { values, negated } => {
            // B2-20: a part's codes are its entries' byte order: each value's code by binary search, a few entries read
            // (their frames faulted on demand), never the payload; a verbatim part's ranks are swept by bytes
            if let DictView::Part(d, _) = dict {
                if d.byte_rank_sorted() {
                    // one code per member of the statement's IN list (data-sized), on the build's owner
                    let mut codes = memledger::ChargedVec::<u32>::new(owner.map(|o| o.0), owner.map_or(memledger::Kind::Region, |o| o.1));
                    codes.with_vec(|v| v.reserve_exact(values.len())); // alloc: Exec charged owner
                    // bounded("other: one value per member of the statement's IN list, which the query text bounds; a binary search each over the dictionary (a few entries read), as PG's ScalarArrayOpExpr tests its members with no poll")
                    for v in values {
                        // the empty value sorts first: code 0 when the index's byte length says it is empty (no entry read:
                        // an entry's first read loads the payload extent whole; granule_source verdicts.rs `code_pred`)
                        let c = match v.is_empty() {
                            true => (d.ncodes() > 0 && d.byte_len_only(0).map_err(TableBuildError::Read)? == 0).then_some(0),
                            false => gravity_dict::lookup::code_of(d.ncodes(), v, &mut |c| d.entry(c).map(|e| e.bytes)).map_err(TableBuildError::Read)?,
                        };
                        if let Some(c) = c {
                            codes.with_vec(|v| v.push(c)); // alloc: Exec charged owner
                        }
                    }
                    return Ok(gravity_dict::Bits::of_codes(dict, &codes, *negated, region)?.header());
                }
            }
            Ok(gravity_dict::Bits::build(dict, region, &mut |v| Ok(values.iter().any(|x| x.as_slice() == raw::view_bytes(v)) != *negated))?.header())
        }
        TableFn::Range { from, to } => {
            // P5-06 / SRC-17: a part's codes are its entries' byte order: the interval's entries are one code run, its ends
            // by binary search (a few entries read, their frames faulted on demand), never the payload; a verbatim part's
            // ranks are swept by bytes
            if let DictView::Part(d, _) = dict {
                if d.byte_rank_sorted() {
                    let n = d.ncodes();
                    let lo = gravity_dict::lookup::lower_bound(n, from, &mut |c| d.entry(c).map(|e| e.bytes)).map_err(TableBuildError::Read)?;
                    let hi = match to {
                        Some(t) => gravity_dict::lookup::lower_bound(n, t, &mut |c| d.entry(c).map(|e| e.bytes)).map_err(TableBuildError::Read)?,
                        None => n,
                    };
                    return Ok(gravity_dict::Bits::of_run(dict, lo, hi, false, region)?.header());
                }
            }
            Ok(gravity_dict::Bits::build(dict, region, &mut |v| {
                let b = raw::view_bytes(v);
                Ok(b >= from.as_slice() && to.as_ref().map_or(true, |t| b < t.as_slice()))
            })?
            .header())
        }
        TableFn::RegexpReplace { pattern, replacement, collation, encoding, global } => {
            if let DictView::Part(d, _) = dict {
                d.prewarm_payload().map_err(TableBuildError::Read)?;
            }
            if regexp_compile_fails(pattern, replacement, *collation, *encoding, *global, fmgr)? {
                // every entry faults: a row re-evaluates and raises PG's error, a row never evaluated raises nothing (D354)
                return Ok(Derived::build(dict, region, &mut |_, _| Err(gravity_dict::table::Halt::Fault(gravity_dict::Fault(1))))?.header());
            }
            match RegexpPlan::of(pattern, replacement, *collation, *encoding) {
                RegexpPlan::Program { prog, tmpl, in_place } => {
                    let mut n = 0u64;
                    let mut memo = gravity_dict::regexp::Memo::new();
                    // `'g'` over an unanchored program: every match (an anchored one's only match is the first)
                    let all = *global && !prog.anchored();
                    let mut f = |v: gravity_dict::View, region: &mut gravity_dict::Region| {
                        n += 1;
                        // a subject the program refuses (its step budget) faults: its rows re-evaluate through the engine
                        let out = match all {
                            true => gravity_dict::regexp::replace_all(&prog, &tmpl, replacement, raw::view_bytes(v), region)?,
                            false => gravity_dict::regexp::replace_first_memo(&prog, &tmpl, replacement, raw::view_bytes(v), region, &mut memo)?,
                        };
                        out.ok_or(gravity_dict::table::Halt::Fault(gravity_dict::Fault(1)))
                    };
                    let payload = match dict {
                        DictView::Part(d, _) if in_place => d.payload_bytes().map_err(TableBuildError::Read)?,
                        _ => None,
                    };
                    let t = match payload {
                        Some(p) => Derived::build_over(dict, region, p, &mut f)?,
                        None => Derived::build(dict, region, &mut f)?,
                    };
                    SCANNED_ENTRIES.fetch_add(n, Ordering::Relaxed);
                    Ok(t.header())
                }
                RegexpPlan::Fmgr { required } => {
                    // bound under the column's collation: a C / POSIX one never reads the backend's default locale (a pool worker has none);
                    // the database encoding armed by the compile check above, which the prefilter below relies on (D354)
                    let call = regexp_call(fmgr, *collation, *global)?;
                    let automaton = automaton(pattern, replacement, *encoding);
                    let mut cx = mcx::MemoryContext::new_bump("gravity dict regexp"); // alloc: Exec
                    let lit = required.map(|(s, e)| &pattern[s..e]);
                    let pair = lit.and_then(gravity_dict::substr::RarePair::new);
                    let images = RegexpImages::new(pattern, replacement, *global); // alloc: Cold
                    let mut arg: Vec<u8> = Vec::with_capacity(1 << 10); // alloc: Exec
                    let mut err: Option<Box<types_error::PgError>> = None;
                    let (mut n, mut skipped, mut by_automaton) = (0u64, 0u64, 0u64);
                    let t = Derived::build(dict, region, &mut |v, region| {
                        if err.is_some() {
                            return Err(gravity_dict::table::Halt::Fault(gravity_dict::Fault(1)));
                        }
                        let b = raw::view_bytes(v);
                        let absent = match (lit, &pair) {
                            (Some(l), Some(p)) => !p.contains(l, b),
                            (Some(l), None) => !b.contains(&l[0]),
                            (None, _) => false,
                        };
                        if absent {
                            skipped += 1;
                            return Ok(v);
                        }
                        n += 1;
                        // D3121: the pattern's own guard (a translated \s \m \M pattern runs RE2 over ASCII entries only)
                        let res = match automaton.as_ref().filter(|re| re.subject_ok(b)) {
                            Some(re) => {
                                by_automaton += 1;
                                // n: 0 every match (`'g'`), 1 the first
                                let r = regexp_alt::replace_text_regexp_re2(cx.mcx(), re, b, replacement, 0, if *global { 0 } else { 1 }).map(|out| Some(region.copy(out.as_slice())));
                                cx.reset();
                                r
                            }
                            None => {
                                arg.clear();
                                arg.extend_from_slice(&varlena_header_4b_u(b.len() as u32).to_le_bytes()); // alloc: Exec
                                arg.extend_from_slice(b); // alloc: Exec
                                images.call(call, &arg).map(|r| {
                                    // a non-null result is copied before the call's next call
                                    (!r.isnull).then(|| region.copy(raw::result_bytes(r.value.as_u64() as *const u8)))
                                })
                            }
                        };
                        match res {
                            Ok(Some(v)) => Ok(v?),
                            Ok(None) => Ok(gravity_dict::View::EMPTY),
                            // an invalid pattern (or a match the engine gives up on) is PG's error on an evaluated row only: the
                            // entry faults, and a row of its code re-evaluates and raises; nothing is raised for rows never evaluated
                            Err(e) if e.sqlstate() == types_error::ERRCODE_INVALID_REGULAR_EXPRESSION => Err(gravity_dict::table::Halt::Fault(gravity_dict::Fault(1))),
                            Err(e) => {
                                err = Some(e);
                                Err(gravity_dict::table::Halt::Fault(gravity_dict::Fault(1)))
                            }
                        }
                    })?;
                    DERIVED_ENTRIES.fetch_add(n, Ordering::Relaxed);
                    AUTOMATON_ENTRIES.fetch_add(by_automaton, Ordering::Relaxed);
                    SKIPPED_ENTRIES.fetch_add(skipped, Ordering::Relaxed);
                    if by_automaton > 0 {
                        fires::fire_shared("dict.regexp_automaton"); // shapecheck fire signal (REGISTRY B2-09, H02)
                    }
                    call.reset_window();
                    match err {
                        Some(e) => Err(TableBuildError::Pg(e)),
                        None => Ok(t.header()),
                    }
                }
            }
        }
    }
}

/// The `Fp128` table of `call`'s value per entry of `dict`, built into `region` in one sweep: the entry's bytes and each
/// constant as a 4-byte-header varlena, the call bound under `collation` with the database encoding armed, and each result
/// fingerprinted as it comes. An entry whose call raised or gave NULL faults: its rows re-evaluate and raise PostgreSQL's
/// error only where it evaluates. A cancel or a resource error is the statement's and aborts the build. The call's result
/// context rewinds before each entry's call (the previous result is fingerprinted by then), as the program's sweep rewinds
/// its region per entry: the build holds one entry's call memory and the table, never a copy of the derived values.
fn call_fingerprints(call: &gravity_program::table_fn::Call, collation: u32, encoding: i32, dict: &DictView<'_>, region: &mut gravity_dict::Region) -> Result<*const gravity_dict::table::Header, TableBuildError> {
    use gravity_dict::Fault;
    if let DictView::Part(d, _) = dict {
        d.prewarm_payload().map_err(TableBuildError::Read)?;
    }
    gravity_fn::typesem::collation::arm_database_encoding(encoding).map_err(TableBuildError::Pg)?;
    let Ok(mut f) = gravity_fn::FmgrCall::bind(call.funcid, call.args.len(), collation) else {
        // a call this thread cannot bind: every entry faults, so each row evaluates it where PostgreSQL does
        return Ok(gravity_dict::Fp128::build(dict, region, &mut |_| Err(Fault(1)))?.header());
    };
    let consts: Vec<Option<Vec<u8>>> = call.args.iter().map(|a| a.as_deref().map(text_image)).collect(); // alloc: Cold
    let nulls = vec![false; call.args.len()]; // alloc: Cold
    let mut datums = vec![datum::Datum::from_usize(0); call.args.len()]; // alloc: Cold
    let mut arg: Vec<u8> = Vec::with_capacity(1 << 10); // alloc: Exec
    let mut entries = 0u64;
    let mut err: Option<Box<types_error::PgError>> = None;
    let t = gravity_dict::Fp128::build(dict, region, &mut |v| {
        if err.is_some() {
            return Err(Fault(1));
        }
        let b = raw::view_bytes(v);
        arg.clear();
        arg.extend_from_slice(&varlena_header_4b_u(b.len() as u32).to_le_bytes()); // alloc: Exec
        arg.extend_from_slice(b); // alloc: Exec
        // bounded("small-fixed: the call's arguments, at most FUNC_MAX_ARGS (100)")
        for (d, c) in datums.iter_mut().zip(&consts) {
            *d = datum::Datum::from_usize(c.as_ref().map_or(arg.as_ptr(), |c| c.as_ptr()) as usize);
        }
        entries += 1;
        f.reset_window();
        match f.call_site(&datums, &nulls) {
            // live until the next entry's rewind: the sweep fingerprints it first
            Ok(r) if !r.isnull => {
                let out = raw::result_bytes(r.value.as_u64() as *const u8);
                Ok(gravity_dict::View { ptr: out.as_ptr(), len: out.len() as u32 })
            }
            Ok(_) => Err(Fault(1)),
            Err(e) if [types_error::ERRCODE_OPERATOR_INTERVENTION, types_error::ERRCODE_INSUFFICIENT_RESOURCES].contains(&types_error::errcode_to_category(e.sqlstate())) => {
                err = Some(e);
                Err(Fault(1))
            }
            Err(_) => Err(Fault(1)),
        }
    });
    f.reset_window();
    if let Some(e) = err {
        return Err(TableBuildError::Pg(e));
    }
    let t = t?;
    DERIVED_ENTRIES.fetch_add(entries, Ordering::Relaxed);
    DERIVED_BUILT.fetch_add(1, Ordering::Relaxed);
    fires::fire_shared("dict.derived_call"); // shapecheck fire signal (REGISTRY H01, STEP-01)
    Ok(t.header())
}

/// The entries derived without a function call (the anchored program) and the fmgr entries the
/// required-literal prefilter skipped, this process.
pub fn regexp_census() -> (u64, u64) {
    (SCANNED_ENTRIES.load(Ordering::Relaxed), SKIPPED_ENTRIES.load(Ordering::Relaxed))
}

static SCANNED_ENTRIES: AtomicU64 = AtomicU64::new(0);
static SKIPPED_ENTRIES: AtomicU64 = AtomicU64::new(0);

/// The entries evaluated by the engine this process, through the fmgr or its automaton (the census beside `tables_built`).
pub fn derived_entries() -> u64 {
    DERIVED_ENTRIES.load(Ordering::Relaxed)
}

/// Of `derived_entries`, those the engine's automaton evaluated with no call (`automaton`).
pub fn automaton_entries() -> u64 {
    AUTOMATON_ENTRIES.load(Ordering::Relaxed)
}

static AUTOMATON_ENTRIES: AtomicU64 = AtomicU64::new(0);

static DERIVED_ENTRIES: AtomicU64 = AtomicU64::new(0);

/// An unfilled table over the dictionary's codes for the program's sweep (`TableFn::Program`): the header, the zeroed data of
/// `elem`'s width per code and a zeroed fault bitmap (`FLAG_FAULTS`; the sweep's done host drops it when no entry faulted).
fn empty_table(dict: &DictView<'_>, region: &mut gravity_dict::Region, elem: u8) -> Result<*const gravity_dict::table::Header, TableBuildError> {
    use gravity_dict::lane::{ELEM_BITS, ELEM_FP128, ELEM_I64};
    use gravity_dict::table::Header;
    let n = gravity_dict::Dict::ncodes(dict) as usize;
    if n >= 1 << gravity_abi::status::FAULT_CODE_BITS {
        return Err(TableBuildError::Region(gravity_dict::BuildError::TooManyCodes));
    }
    let bytes = match elem {
        ELEM_BITS => n.div_ceil(64) * 8,
        ELEM_I64 => n * 8,
        ELEM_FP128 => n * 16,
        _ => return Err(TableBuildError::Pg(crate::pg_err("gravity: a program table of an element the program cannot fill"))),
    };
    let hdr = region.alloc_zeroed(core::mem::size_of::<Header>(), 8)? as *mut Header;
    let data = region.alloc_zeroed(bytes.max(8), 8)?;
    let faults = region.alloc_zeroed((n.div_ceil(64) * 8).max(8), 8)? as *const u64;
    raw::program_header(hdr, elem, n as u32, data, faults, region.base());
    Ok(hdr)
}

/// `spec` over `dict` in an owned buffer sized from the dictionary's resident payload (a statement's shared table).
pub fn build_shared(spec: &TableSpec, dict: &DictView<'_>, fmgr: &mut Option<gravity_fn::FmgrCall>) -> Result<FpTable, TableBuildError> {
    build_shared_for(spec, dict, fmgr, None)
}

/// `build_shared` with the table's buffer and the build's working memory charged to `owner` from the build to the table's
/// drop (a verbatim part's view: its source's when `owner` is none).
pub fn build_shared_for(spec: &TableSpec, dict: &DictView<'_>, fmgr: &mut Option<gravity_fn::FmgrCall>, owner: BuildOwner) -> Result<FpTable, TableBuildError> {
    let owner = owner.or(dict.owner());
    // the steps label the build's stretches; the prewarm and the build poll inside (pgrc_read's ensure_all, the walks)
    let _step = cancelpoll::step("text_hosts::build_shared");
    if let DictView::Part(d, _) = dict {
        let _step = cancelpoll::step("text_hosts::build_shared: the payload's prewarm");
        d.prewarm_payload().map_err(TableBuildError::Read)?;
    }
    let n = gravity_dict::Dict::ncodes(dict) as usize;
    let in_place = match (&spec.func, dict) {
        (TableFn::RegexpReplace { pattern, replacement, collation, encoding, .. }, DictView::Part(..)) => matches!(RegexpPlan::of(pattern, replacement, *collation, *encoding), RegexpPlan::Program { in_place: true, .. }),
        _ => false,
    };
    let payload = match dict {
        _ if in_place || matches!(spec.func, TableFn::DerivedFingerprint { .. }) => 0,
        DictView::Part(d, _) => d.resident_payload_bytes() as usize,
        DictView::Ranked { .. } => n * 128,
    };
    let bytes = match spec.func {
        TableFn::Like { .. } | TableFn::Eq { .. } | TableFn::Range { .. } | TableFn::EqBound { .. } | TableFn::LikeBound { .. } | TableFn::RangeBound { .. } => 64 + n.div_ceil(8) + (1 << 12),
        TableFn::Faults => 64 + 2 * n.div_ceil(64) * 8 + (1 << 12),
        _ => 64 + 48 * n + payload + (1 << 16),
    };
    let _step = cancelpoll::step("text_hosts::build_shared: the table's build");
    let t = build_owned_held(bytes, Held::By(owner), &mut |region| build_table_for(spec, dict, region, fmgr, owner))?;
    if matches!(spec.func, TableFn::DerivedFingerprint { .. }) {
        DERIVED_BUILT.fetch_add(1, Ordering::Relaxed);
    }
    Ok(t)
}

/// textkeys step 3 (`TableFn::Canon`): the canonical image table of `dict` (a statement's shared table), each long entry's
/// image read from `raw` (the part's resident `Fp128` table over the same renders, trimmed when `bpchar`) when given, else
/// hashed in the build; the buffer and the build charged to `owner` (a verbatim part's view: its source's when none).
pub fn build_canon_for(dict: &DictView<'_>, raw: Option<&FpTable>, bpchar: bool, owner: BuildOwner) -> Result<FpTable, TableBuildError> {
    let owner = owner.or(dict.owner());
    let _step = cancelpoll::step("text_hosts::build_canon_for");
    fp_seed();
    if let DictView::Part(d, _) = dict {
        d.prewarm_payload().map_err(TableBuildError::Read)?;
    }
    let n = gravity_dict::Dict::ncodes(dict) as usize;
    let raw = raw.map(FpTable::table);
    build_owned_held(64 + 16 * n + (1 << 16), Held::By(owner), &mut |region| {
        Ok(gravity_dict::Fp128::build_canon(dict, region, &mut |v| Ok(if bpchar { bpchar_trimmed(v) } else { v }), raw.as_ref())?.header())
    })
}

static DERIVED_BUILT: AtomicU64 = AtomicU64::new(0);

/// The derived fingerprint tables built this process (`build_shared`, `call_fingerprints`): one per part per statement that
/// declares one (a call table: per part per source that reads it).
pub fn derived_tables_built() -> u64 {
    DERIVED_BUILT.load(Ordering::Relaxed)
}

/// `build` into an owned buffer of `64 + 48 × ncodes + payload` bytes, lazily zeroed, grown on `RegionFull` (the build repeated).
pub fn build_owned(ncodes: usize, payload: usize, build: &mut dyn FnMut(&mut gravity_dict::Region) -> Result<*const gravity_dict::table::Header, TableBuildError>) -> Result<FpTable, TableBuildError> {
    build_owned_bytes(64 + 48 * ncodes + payload + (1 << 16), build)
}

pub fn build_owned_bytes(bytes: usize, build: &mut dyn FnMut(&mut gravity_dict::Region) -> Result<*const gravity_dict::table::Header, TableBuildError>) -> Result<FpTable, TableBuildError> {
    build_owned_held(bytes, Held::By(None), build)
}

/// `build_owned_bytes` with the buffer charged as `h` says from its take (`take_held`).
fn build_owned_held(bytes: usize, h: Held, build: &mut dyn FnMut(&mut gravity_dict::Region) -> Result<*const gravity_dict::table::Header, TableBuildError>) -> Result<FpTable, TableBuildError> {
    let mut words = bytes.div_ceil(8);
    // bounded("other: a retry only when the region was short, grown at least twofold and by what the build asked for; each build polls its own walk")
    loop {
        let mut hb = take_held(words, h);
        let mut region = raw::region(&mut hb.buf);
        match build(&mut region) {
            Ok(hdr) => return Ok(FpTable { cached: Mutex::new(hb.held), used: region.used() as u64, buf: hb.buf, hdr, ranks: None, off: None, renders: false, pins: Mutex::new(None) }),
            Err(TableBuildError::Region(gravity_dict::BuildError::RegionFull { needed })) => words = (words * 8 + needed + (1 << 16)).div_ceil(8).max(words * 2),
            Err(e) => return Err(e),
        }
    }
}

pub fn text_image(b: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(b.len() + 4); // alloc: Cold
    v.extend_from_slice(&varlena_header_4b_u(b.len() as u32).to_le_bytes());
    v.extend_from_slice(b);
    v
}

/// D3288 (C): a verbatim part's LIKE table by ONE scan of each granule's decoded value bytes (its 4B-U images back to
/// back in the scratch arena), a hit's row resolved by the row spans and the row's verdict set on its code; rows of a
/// code hold its bytes, so any row decides the code. A granule whose images do not lie ascending is decided row by row.
fn ranked_scan_bits(m: &gravity_dict::like::Matcher<'_>, dict: &DictView<'_>, ranks: &Ranks, cur: &core::cell::RefCell<&mut StreamCursor<'static>>, scratch: &core::cell::RefCell<&mut TextScratch>, region: &mut gravity_dict::Region) -> Result<gravity_dict::Bits, TableBuildError> {
    fires::fire_shared("like.granule_scan"); // shapecheck fire signal (D3288): a verbatim part's LIKE table by granule scans
    let (mut cur, mut scratch) = (cur.borrow_mut(), scratch.borrow_mut());
    let mut failed = None;
    let mut step = cancelpoll::step("text_hosts ranked_scan_bits: a verbatim part's LIKE scan");
    let bits = gravity_dict::Bits::of_hits(dict, m.is_negated(), region, &mut |set| {
        for lg in ranks.granules() {
            if step.tick().is_err() {
                return failed = Some(ReadError::Canceled);
            }
            let (rows, all) = match scratch.decode(&mut cur, lg) {
                Ok(r) => r,
                Err(e) => return failed = Some(e),
            };
            let codes = ranks.granule_codes(lg);
            match scratch.row_spans(rows, all) {
                Some(end) => {
                    let sc = &**scratch;
                    let payload = raw::arena_bytes(&sc.arena, end);
                    m.scan_entries(&RowSpans(&sc.spans), payload, &mut |row| set(codes[row as usize]));
                }
                None => {
                    // `matches` answers after the negation; `of_hits` takes the verdict before it
                    // bounded("small-fixed: one granule's rows (at most GRANULE_ROWS, 8192), a LIKE test each, as PG's per-row test; the granule loop ticks")
                    for i in 0..rows {
                        if scratch.live(all, i) && m.matches(raw::payload(scratch.datums[i])).unwrap_or(false) != m.is_negated() {
                            set(codes[i]);
                        }
                    }
                }
            }
        }
    })?;
    match failed {
        Some(e) => Err(TableBuildError::Read(e)),
        None => Ok(bits),
    }
}

/// A granule's rows as entries: each live row's value bytes in the scratch arena, a NULL row an empty span.
struct RowSpans<'a>(&'a [(usize, usize)]);

impl gravity_dict::like::EntryIndex for RowSpans<'_> {
    fn ncodes(&self) -> u32 {
        self.0.len() as u32
    }

    fn span(&self, code: u32) -> (usize, usize) {
        self.0[code as usize]
    }
}

#[derive(Debug)]
pub enum TableBuildError {
    Region(gravity_dict::BuildError),
    Pg(Box<types_error::PgError>),
    Read(ReadError),
}

impl From<gravity_dict::BuildError> for TableBuildError {
    fn from(e: gravity_dict::BuildError) -> TableBuildError {
        TableBuildError::Region(e)
    }
}

/// Re-evaluate entry `bytes` of a table that faulted at the gather (the runner's arm): the same
/// function, serially, so PostgreSQL's own error text is raised for the row that selected it.
pub fn reevaluate(spec: &TableSpec, bytes: &[u8]) -> PgResult<()> {
    match &spec.func {
        TableFn::Like { pattern, ilike, negated: _ } => {
            let mut region_buf = vec![0u64; 1024]; // alloc: Cold
            let mut region = raw::region(&mut region_buf);
            let mode = if *ilike { gravity_dict::like::Mode::ILike } else { gravity_dict::like::Mode::Like };
            let m = gravity_dict::like::Matcher::new(pattern, mode, &mut region).map_err(|_| crate::pg_err("gravity: LIKE pattern"))?;
            match m.matches(bytes) {
                Ok(_) => Ok(()),
                Err(_) => Err(Box::new(types_error::PgError::error("LIKE pattern must not end with escape character".to_string()).with_sqlstate(types_error::ERRCODE_INVALID_ESCAPE_SEQUENCE))), // alloc: Cold
            }
        }
        TableFn::RegexpReplace { pattern, replacement, collation, global, .. } | TableFn::DerivedFingerprint { pattern, replacement, collation, global, .. } => {
            let (oid, nargs) = regexp_fn(*global);
            let mut call = gravity_fn::FmgrCall::bind(oid, nargs, *collation)?;
            RegexpImages::new(pattern, replacement, *global).call(&mut call, &text_image(bytes)).map(|_| ()) // alloc: Cold
        }
        TableFn::DerivedCall { call, collation, .. } => {
            let mut f = gravity_fn::FmgrCall::bind(call.funcid, call.args.len(), *collation)?;
            let args: Vec<Vec<u8>> = call.args.iter().map(|a| text_image(a.as_deref().unwrap_or(bytes))).collect(); // alloc: Cold
            let datums: Vec<datum::Datum> = args.iter().map(|a| datum::Datum::from_usize(a.as_ptr() as usize)).collect(); // alloc: Cold
            f.call_site(&datums, &vec![false; datums.len()]).map(|_| ()) // alloc: Cold
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod d3887_tests {
    use super::Store;

    /// The rows' sort `row_codes` runs: `cancelpoll::sort_by` under a step; false when canceled.
    fn sorted_under_a_step(order: &mut Vec<u32>, cmp: impl Fn(&u32, &u32) -> core::cmp::Ordering) -> bool {
        let mut scratch = vec![0u32; order.len()]; // alloc: Cold
        cancelpoll::sort_by(&mut cancelpoll::step("test sort"), order, &mut scratch, cmp).is_ok()
    }
    use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

    /// D3887: a verbatim part's row sort and its fingerprint walk stop mid-build once the statement is canceled (the byte
    /// set after some comparisons / entries), and sort exactly as before when it is not.
    #[test]
    fn the_row_sort_and_the_fingerprint_walk_stop_mid_build_on_a_cancel() {
        let keys: Vec<u64> = (0..300_000u64).map(|i| i.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 7).collect();
        let byte = AtomicU8::new(0);
        let mut order: Vec<u32> = (0..keys.len() as u32).collect();
        let mut want = order.clone();
        want.sort_by_key(|&i| keys[i as usize]);
        assert!(cancelpoll::scoped(&byte, || sorted_under_a_step(&mut order, |&a, &b| keys[a as usize].cmp(&keys[b as usize]))));
        assert_eq!(order.iter().map(|&i| keys[i as usize]).collect::<Vec<_>>(), want.iter().map(|&i| keys[i as usize]).collect::<Vec<_>>());
        // the cancel lands after 100k comparisons: the sort returns at its next run or merge
        let seen = AtomicUsize::new(0);
        let mut order: Vec<u32> = (0..keys.len() as u32).collect();
        let done = cancelpoll::scoped(&byte, || {
            sorted_under_a_step(&mut order, |&a, &b| {
                if seen.fetch_add(1, Ordering::Relaxed) == 100_000 {
                    byte.store(cancelpoll::ABORT_BIT, Ordering::Relaxed);
                }
                keys[a as usize].cmp(&keys[b as usize])
            })
        });
        assert!(!done, "canceled mid-sort");
        assert!(seen.load(Ordering::Relaxed) < 2_000_000, "it stopped well before the whole sort ({} comparisons)", seen.load(Ordering::Relaxed));
        // the walk stops at its next chunk of entries
        byte.store(0, Ordering::Relaxed);
        let bytes = vec![7u8; 100_000]; // alloc: Exec
        let ents: Vec<(u32, u32)> = (0..100_000u32).map(|i| (i, 1)).collect();
        let store = Store { bytes: &bytes, ents: &ents, stopped: core::cell::Cell::new(false) };
        let mut n = 0u32;
        cancelpoll::scoped(&byte, || {
            gravity_dict::Dict::entries(&store, &mut |_, _| {
                n += 1;
                if n == 10_000 {
                    byte.store(cancelpoll::ABORT_BIT, Ordering::Relaxed);
                }
            })
        });
        assert!(store.stopped.get() && n < 20_000, "{n} entries walked");
    }
}

#[cfg(test)]
mod d4233_tests {
    use core::sync::atomic::{AtomicU8, Ordering};
    use pgrc_read::ReadError;

    /// A dictionary over `vals` (code `c` is `vals[c]`) whose walk raises the statement's abort bit as it hands out code
    /// `cancel_at`, after the walk's own tick for it: the cancel lands while that code's image is laid.
    struct Vals<'a> {
        vals: Vec<Vec<u8>>,
        byte: &'a AtomicU8,
        cancel_at: Option<usize>,
    }

    impl gravity_dict::Dict for Vals<'_> {
        fn ncodes(&self) -> u32 {
            self.vals.len() as u32
        }

        fn entries(&self, f: &mut dyn FnMut(u32, gravity_dict::View)) {
            self.vals.iter().enumerate().for_each(|(c, v)| {
                if self.cancel_at == Some(c) {
                    self.byte.store(cancelpoll::ABORT_BIT, Ordering::Release);
                }
                f(c as u32, gravity_dict::View::of(v));
            });
        }
    }

    /// Code `c`'s image in a render buffer: its 4B-U header's length, then its bytes.
    fn image(buf: &[u64], at: u32) -> &[u8] {
        let b = crate::ops::scan::pgrc::raw::words_as_bytes(&buf[at as usize..]);
        let len = (u32::from_le_bytes([b[0], b[1], b[2], b[3]]) >> 2) as usize;
        &b[4..len]
    }

    /// D4233: a cancel that lands while a long value's image is laid stops the render table there (an image is laid
    /// `PIECE` bytes a tick, its buffer's growth too); with none the images are every value's, from a buffer that grew.
    #[test]
    fn a_cancel_while_a_long_values_image_is_laid_stops_the_render_table() {
        let vals = vec![b"a".to_vec(), vec![7u8; 3 * cancelpoll::PIECE + 5], b"bcd".to_vec()]; // alloc: Exec
        let byte = AtomicU8::new(0);
        let d = Vals { vals: vals.clone(), byte: &byte, cancel_at: None }; // alloc: Exec
        let (_, buf, off) = cancelpoll::scoped(&byte, || super::lay_renders(&d, 3, 1, &mut super::ScratchGate::new())).expect("no cancel");
        assert_eq!([image(&buf, off[0]), image(&buf, off[1]), image(&buf, off[2])], [&vals[0][..], &vals[1][..], &vals[2][..]]);
        let d = Vals { vals, byte: &byte, cancel_at: Some(1) };
        let r = cancelpoll::scoped(&byte, || super::lay_renders(&d, 3, 1 << 20, &mut super::ScratchGate::new()).map(|_| ()));
        assert!(matches!(r, Err(ReadError::Canceled)), "the long value's image stopped at its first piece");
    }
}

#[cfg(test)]
pub(super) mod owner_tests {
    use memledger::charge::usage;
    use memledger::{Acct, Kind, Owner};

    /// On a new thread bound to backend `procno`, inside a statement: `f(account, a slot owner of kind `kind` under it)`.
    pub(in crate::dict) fn in_statement(procno: u32, kind: Kind, f: impl FnOnce(Acct, Owner) + Send + 'static) {
        std::thread::spawn(move || {
            memledger::bind_backend(procno, 9000 + procno);
            memledger::statement_begin();
            let a = Acct::backend(procno);
            let st = Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
            let slot = memledger::charge::op_begin(st, kind as u32, 0);
            f(a, slot);
            memledger::charge::op_end(slot);
            let _ = memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// A growing build buffer is its owner's while it lives and leaves at its drop; a growth frees the old buffer (the
    /// owner holds the grown one alone, never both, as `Step::reserve` frees a `Vec`'s), and with no owner none is charged.
    #[test]
    fn a_growing_build_buffer_is_its_owners_and_a_growth_frees_the_old_one() {
        in_statement(3971, Kind::Sort, |_, slot| {
            let before = usage(slot).held;
            let mut step = cancelpoll::step("test: a build buffer");
            {
                let mut g: super::Grow<u8> = super::Grow::new(Some((slot, Kind::Sort)));
                assert!(g.reserve(&mut step, 4 << 20).is_ok() && g.extend_from_slice(&[7u8; 4 << 20]).is_ok());
                let one = usage(slot).held - before;
                assert!(one >= 4 << 20 && one < (4 << 20) + (64 << 10), "the 4 MiB buffer on its owner: {one}");
                assert!(g.reserve(&mut step, 1).is_ok());
                let two = usage(slot).held - before;
                assert!(two >= 8 << 20 && two < (8 << 20) + (64 << 10), "the grown 8 MiB buffer alone, the old one freed: {two}");
                assert_eq!((g.len(), g.capacity()), (4 << 20, 8 << 20));
                assert!(g.as_slice().iter().all(|&b| b == 7), "the elements moved");
            }
            assert_eq!(usage(slot).held, before, "gone at the drop");
            let mut g: super::Grow<u64> = super::Grow::new(None);
            assert!(g.reserve(&mut step, 1 << 18).is_ok() && g.push(1).is_ok());
            assert_eq!(usage(slot).held, before, "no owner charged");
        });
    }

    /// A statement's table is on its building owner from its take to its drop; when the fp cache takes a table, its
    /// building owner's charge ends and the cache's opens (the bytes on one owner at a time).
    #[test]
    fn a_tables_buffer_is_its_builders_until_the_cache_takes_it() {
        in_statement(3972, Kind::HashAgg, |_, slot| {
            let built = |o| super::build_owned_held(1 << 20, super::Held::By(o), &mut |_| Ok(core::ptr::null())).ok().expect("built");
            let before = usage(slot).held;
            let t = built(Some((slot, Kind::HashAgg)));
            let held = usage(slot).held - before;
            assert!(held >= 1 << 20 && held as usize == t.buf.capacity() * 8, "the buffer's capacity on its builder: {held}");
            drop(t);
            assert_eq!(usage(slot).held, before, "back at the table's drop");
            let t = built(Some((slot, Kind::HashAgg)));
            t.take_into_cache();
            assert_eq!(usage(slot).held, before, "the cache's now");
            assert!(t.cached.lock().unwrap().as_ref().is_some_and(|c| c.owner() == super::FP_OWNER && c.committed() == t.alloc_bytes()));
            let n = super::build_owned_bytes(1 << 20, &mut |_| Ok(core::ptr::null())).ok().expect("built");
            assert!(n.cached.lock().unwrap().is_none() && usage(slot).held == before, "no owner: no charge");
            // CACHE-CHARGE: a resident image's buffer is the fp tables' from its take (its scope's charge), fitted to what
            // the table allocates when the cache takes it
            let r = super::build_owned_held(1 << 20, super::Held::Resident, &mut |_| Ok(core::ptr::null())).ok().expect("built");
            assert!(r.cached.lock().unwrap().as_ref().is_some_and(|c| c.owner() == super::FP_OWNER), "the fp tables' from the take");
            assert_eq!(usage(slot).held, before, "not the slot's");
            r.take_into_cache();
            assert!(r.cached.lock().unwrap().as_ref().is_some_and(|c| c.owner() == super::FP_OWNER && c.committed() == r.alloc_bytes()), "fitted at the insert");
        });
    }

    /// D4529: a cached table's charge is what its lists allocate, not their lengths: the representatives a build pushes
    /// keep their capacity past their length, and that capacity stays the cache's, not the building thread's.
    #[test]
    fn a_cached_tables_charge_covers_its_lists_capacity() {
        in_statement(4529, Kind::HashAgg, |_, slot| {
            let before = usage(slot).held;
            let mut reps: Vec<u32> = Vec::with_capacity(1 << 16); // alloc: Cold
            reps.extend(0..1000u32);
            let ranks = super::Ranks { codes: vec![0; 1000], starts: vec![0, 1000], reps, empty: false, kept: None, salt: 0, lens: None, render_words: 0 }; // alloc: Cold
            let t = super::FpTable { buf: vec![0; 8].into(), hdr: core::ptr::null(), ranks: Some(ranks), used: 64, off: None, renders: false, cached: pgsync::Mutex::new(None), pins: pgsync::Mutex::new(None) }; // alloc: Cold
            assert!(t.alloc_bytes() >= t.bytes() + 4 * ((1 << 16) - 1000), "the representatives' capacity counts");
            t.take_into_cache();
            assert!(t.cached.lock().unwrap().as_ref().is_some_and(|c| c.owner() == super::FP_OWNER && c.committed() == t.alloc_bytes()), "the cache's charge is the allocation");
            drop(t);
            assert_eq!(usage(slot).held, before, "not the statement's");
        });
    }

    /// A render memo's codes are on its owner once armed, at its capacity, and follow a new owner; none at its drop.
    #[test]
    fn a_render_memo_is_charged_at_its_capacity_to_its_owner() {
        in_statement(3973, Kind::HashAgg, |a, slot| {
            let before = usage(slot).held;
            let mut m = super::RenderMemo::new();
            m.set_owner(Some((slot, Kind::HashAgg)));
            assert_eq!(usage(slot).held, before, "nothing armed yet");
            for _ in 0..super::RENDER_MEMO_AFTER {
                let _ = m.get(0, 1, 0, 100_000, 7, false);
            }
            let held = usage(slot).held - before;
            assert_eq!(held as usize, m.memo.capacity() * 16, "the memo's capacity on its owner");
            assert!(held >= 1_600_000);
            let st = Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
            let on_st = usage(st).held;
            m.set_owner(Some((st, Kind::Region)));
            assert_eq!((usage(slot).held, usage(st).held - on_st), (before, held), "moved to the new owner");
            drop(m);
            assert_eq!(usage(st).held, on_st, "none at its drop");
        });
    }

    /// D3324 R1: renders that alternate two verbatim parts keep each part's count and table across the switches: each
    /// part earns its render table after `RENDER_WALK_AFTER` of its own renders, and a table taken comes back when the
    /// renders return to its part; a new generation of a part starts over.
    #[test]
    fn d3324_a_verbatim_parts_renders_survive_part_switches() {
        let mut m = super::RenderMemo::new();
        for i in 0..2 * super::RENDER_WALK_AFTER - 2 {
            assert_eq!(m.get((i % 2) as usize, 1, 0, 10, 3, true), None, "no table before it is earned");
            assert!(!m.walk_due(), "part {} has {} renders", i % 2, i / 2 + 1);
        }
        assert_eq!(m.get(0, 1, 0, 10, 3, true), None);
        assert!(m.walk_due(), "part 0's own renders earned its table across the switches");
        let mut buf = vec![0u64; 2]; // alloc: Cold
        buf[0] = (4 + 3) << 2 | u64::from_le_bytes(*b"\0\0\0\0abc\0"); // a 4B-U varlena of 3 bytes
        let t = std::sync::Arc::new(super::FpTable::of_renders(buf, vec![u32::MAX, u32::MAX, u32::MAX, 0].into_boxed_slice())); // alloc: Cold
        m.take_table(std::sync::Arc::clone(&t));
        assert_eq!(m.get(0, 1, 0, 10, 3, true).map(|h| h.1), Some(3), "the table renders");
        assert_eq!(m.get(1, 1, 0, 10, 3, true), None, "part 1 has no table yet");
        assert!(m.walk_due(), "part 1's count came back too");
        assert_eq!(m.get(0, 1, 0, 10, 3, true).map(|h| h.1), Some(3), "part 0's table came back at the switch");
        assert_eq!(std::sync::Arc::strong_count(&t), 2, "moved, not cloned");
        assert_eq!(m.get(0, 2, 0, 10, 3, true), None, "a new generation of the part starts over");
        assert!(!m.walk_due());
        assert_eq!(std::sync::Arc::strong_count(&t), 2, "the left table stays held for the statement");
        drop(m);
        assert_eq!(std::sync::Arc::strong_count(&t), 1);
    }

    /// D4536: a build-pool buffer taken under the fp tables' cache scope is theirs from the take: the pool's charge moves
    /// to the scope's, and the building backend's unattributed bytes stay where they were (they rose by the whole buffer
    /// until the cache's insert).
    #[test]
    fn d4536_a_pooled_buffer_taken_under_a_scope_is_the_scopes_from_its_take() {
        in_statement(4536, Kind::HashAgg, |a, _| {
            // a size the other tests' takes do not fit (a kept buffer serves takes of a quarter of it and up)
            let words = 1_000_003;
            let buf = vec![0u64; words]; // alloc: Exec
            let at = buf.as_ptr();
            super::reuse::give(buf);
            let un = || memledger::unattr::measure(a).expect("a reading").0;
            let before = un();
            let mut scope = memledger::arena::cache_scope_for(super::FP_OWNER, Kind::Region);
            let got = super::reuse::take_for(words, Some(&mut scope));
            assert_eq!(got.as_ptr(), at, "the kept buffer");
            let after = un();
            assert!((after - before).abs() < 1 << 20, "not the backend's unattributed bytes: {before} -> {after}");
            let c = scope.finish().expect("a holder scope's charge");
            assert_eq!((c.owner(), c.committed()), (super::FP_OWNER, (words * 8) as u64), "the pool's charge is the scope's");
            drop(c);
            drop(got);
        });
    }
}
