//! Ingest normalization + column accumulation.
//!
//! **The detoast-on-ingest law** (charter §7, spec §3): byte-exactness is
//! owed on the DETOASTED, decompressed image — the toast form is a
//! non-surface. Every varlena input is normalized here to its plain payload
//! bytes before anything else sees it, so toasted and inline presentations
//! of the same value converge to identical part bytes (the M3-D pinning
//! test). Arms:
//!
//! - 4B uncompressed → payload borrowed directly;
//! - 1B short header → payload borrowed past the 1-byte header;
//! - 4B inline-compressed → decompressed here: pglz via the pure `pglz`
//!   port; lz4 toast raises the tree's standing typed refusal (built
//!   without lz4 toast, C parity — the same posture as `detoast`/
//!   `heaptoast`);
//! - external/indirect (1B_E tag) → the [`ExternalDetoast`] capability
//!   (installed by M3-H over the real `detoast` seam machinery; COPY — the
//!   only M3 ingest under O-M3-1(a) — feeds fresh inline datums, so the
//!   default [`NoExternalDetoast`] refusal is never reachable from the M3
//!   surface).
//!
//! **[`ColBuffer`]** accumulates one column of one part: datum words for
//! byval classes, an 8-aligned image heap for byref classes (varlena images
//! stored VARLENA-SHAPED — the StrView §7b invariant holds from the
//! accumulation buffer onward), a validity bitset, and the exact chunk
//! stats (constancy, value bytes, oversize count) that feed the election
//! plus the O-10 logical column hash. Granule slices materialize
//! [`EncodeInput`]s for the seal loop at the part's ELECTED grain (SB-10);
//! every ladder grain divides by 64 exactly (1024/64 = 16 … 8192/64 = 128),
//! so per-granule validity is a word-aligned slice of the part bitset at
//! any grain.

use pgrc_format::abi::EncodeInput;
use pgrc_format::bank::LogicalColHash;
use pgrc_format::class::{ColSchema, StorageClass};
use pgrc_format::geom::{GranuleGrain, OVERSIZE_THRESHOLD};
use pgrc_format::wire::varlena_header_4b_u;

use crate::held::HeldCharge;
use crate::{WriteError, WriteResult};

/// One ingest datum, in the safe borrowed shape (M3-H unpacks PG datum
/// arrays into this at the AM boundary; tests build it directly).
#[derive(Debug, Clone, Copy)]
pub enum RawDatum<'a> {
    Null,
    /// Byval classes: the datum word in spec §6.7 extension convention.
    Word(u64),
    /// Byref classes: Fixed = exactly `len` raw bytes; Varlena = a raw
    /// varlena image in ANY toast form (normalized here).
    Bytes(&'a [u8]),
}

/// Capability for external/indirect toast pointers (heap access — a passed
/// capability, never a global). `out` receives the fully detoasted,
/// decompressed PAYLOAD bytes (no varlena header).
pub trait ExternalDetoast {
    fn detoast_external(&mut self, image: &[u8], out: &mut Vec<u8>) -> WriteResult<()>;
}

/// The M3 default: typed refusal (unreachable from COPY ingest, the only
/// M3 DML surface per O-M3-1(a)).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoExternalDetoast;

impl ExternalDetoast for NoExternalDetoast {
    fn detoast_external(&mut self, _image: &[u8], _out: &mut Vec<u8>) -> WriteResult<()> {
        Err(WriteError::Refused {
            what: "external toast pointer without a detoast capability",
        })
    }
}

/// Which arm normalization took (surfaced for the convergence pin test).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarlenaForm {
    Plain4B,
    Short1B,
    CompressedPglz,
    External,
}

const VARATT_MASK_4B: u8 = 0x03;
const VARATT_4B_U: u8 = 0x00;
const VARATT_4B_C: u8 = 0x02;
/// tcinfo top-2-bit compression methods (C toast_compression.h).
const TOAST_PGLZ: u32 = 0;
const TOAST_LZ4: u32 = 1;

/// Normalize a raw varlena image to detoasted, decompressed payload bytes.
/// Returns the borrowed payload when no transformation is needed, else
/// fills `scratch` and returns a slice of it (caller keeps `scratch` alive).
pub fn normalize_varlena<'a>(
    image: &'a [u8],
    ext: &mut dyn ExternalDetoast,
    scratch: &'a mut Vec<u8>,
) -> WriteResult<(&'a [u8], VarlenaForm)> {
    let b0 = *image.first().ok_or(WriteError::Contract {
        detail: "empty varlena image",
    })?;
    if b0 == 0x01 {
        // 1B_E: external / indirect toast pointer.
        scratch.clear();
        ext.detoast_external(image, scratch)?;
        return Ok((scratch.as_slice(), VarlenaForm::External));
    }
    if b0 & 0x01 == 0x01 {
        // 1B short header: total size (incl. header) in bits 1..7.
        let total = ((b0 >> 1) & 0x7F) as usize;
        if total < 1 || total > image.len() {
            return Err(WriteError::Contract {
                detail: "short varlena size out of bounds",
            });
        }
        return Ok((&image[1..total], VarlenaForm::Short1B));
    }
    if image.len() < 4 {
        return Err(WriteError::Contract {
            detail: "varlena image shorter than its header",
        });
    }
    let header = u32::from_le_bytes(image[..4].try_into().expect("len 4"));
    let total = (header >> 2) as usize;
    if total < 4 || total > image.len() {
        return Err(WriteError::Contract {
            detail: "varlena size out of bounds",
        });
    }
    match b0 & VARATT_MASK_4B {
        VARATT_4B_U => Ok((&image[4..total], VarlenaForm::Plain4B)),
        VARATT_4B_C => {
            // 4B compressed: [header][va_tcinfo][compressed bytes].
            if total < 8 {
                return Err(WriteError::Contract {
                    detail: "compressed varlena shorter than tcinfo",
                });
            }
            let tcinfo = u32::from_le_bytes(image[4..8].try_into().expect("len 4"));
            let rawsize = (tcinfo & 0x3FFF_FFFF) as usize;
            let method = tcinfo >> 30;
            match method {
                TOAST_PGLZ => {
                    scratch.clear();
                    pgrc_format::try_resize(scratch, rawsize, 0, "ingest")?;
                    let n = pglz::pglz_decompress_slice(&image[8..total], scratch, true)
                        .ok_or(WriteError::Contract {
                            detail: "corrupt pglz toast payload",
                        })?;
                    if n != rawsize {
                        return Err(WriteError::Contract {
                            detail: "pglz toast rawsize mismatch",
                        });
                    }
                    Ok((scratch.as_slice(), VarlenaForm::CompressedPglz))
                }
                TOAST_LZ4 => Err(WriteError::Refused {
                    what: "compression method lz4 not supported",
                }),
                _ => Err(WriteError::Contract {
                    detail: "invalid toast compression method",
                }),
            }
        }
        _ => Err(WriteError::Contract {
            detail: "unrecognized varlena header form",
        }),
    }
}

/// Canonical value-byte width of a word class (spec §18.1).
fn word_canonical_width(class: StorageClass) -> usize {
    match class {
        StorageClass::ByvalWord { width, .. } => width as usize,
        StorageClass::F32 => 4,
        StorageClass::F64 => 8,
        StorageClass::Bool => 1,
        _ => unreachable!("word classes only"),
    }
}

// ---------------------------------------------------------------------------
// SEAL-SPEED-2 D2: the inherited-dictionary side channel
// ---------------------------------------------------------------------------

/// A source dictionary inherited from a foreign file's own structure (D2:
/// parquet dict pages). Entries are the source's payload bytes in SOURCE
/// order — code `i` names entry `i`; nothing here is sorted or deduplicated
/// across sources (that is the seal's merge). Implemented by the ingest
/// driver over its format reader (layering: this crate never sees parquet).
pub trait DictEntries: Send + Sync {
    fn entry_count(&self) -> u32;
    /// Entry payload bytes of `code` (caller contract: `code < entry_count`).
    fn entry(&self, code: u32) -> &[u8];
}

/// Per-row sidecar slot: `(source_idx << 32) | code` for a row decoded off
/// a source dictionary; [`DICT_ROW_PLAIN`] for a row with no code (PLAIN
/// fallback pages — the hybrid law hashes those — and null rows, which the
/// merge skips through validity anyway).
pub const DICT_ROW_PLAIN: u64 = u64::MAX;

/// The inherited-dictionary side channel of one column of one part: the
/// source dictionaries covering the part's rows plus each row's
/// `(source, code)`. VALID only when it covers every row (`rows.len() ==
/// ColBuffer::rows`) — a part spliced from any chunk without the channel
/// drops it whole (deterministic: channel presence is a pure function of
/// the input file bytes, never of the claim schedule). Sidecar bytes are
/// measurement-invisible: nothing here ever reaches part bytes except
/// through the seal's canonical-form dict build, which is proven
/// byte-identical to the rebuild path (the D2 twin gates).
pub struct DictSide {
    pub sources: Vec<std::sync::Arc<dyn DictEntries>>,
    /// Row-dense, one slot per part row.
    pub rows: Vec<u64>,
}

/// One column of one part, accumulated (whole-part buffering — the design
/// forced by global dictionaries + per-stream elections; see crate docs).
pub struct ColBuffer {
    /// CONVERT W3-2b: the load's statement owner's charge on [`Self::held_bytes`] (none for [`ColBuffer::new`]),
    /// synced where the blocks grow. Declared first: it drops before the blocks are freed.
    charge: HeldCharge,
    pub schema: ColSchema,
    /// Byval classes: datum words. Byref classes: heap offsets of the image
    /// start (varlena: the 4-B header). Null rows: 0.
    words: Vec<u64>,
    /// Byref image heap, entries 8-aligned; varlena entries varlena-shaped.
    heap: Vec<u8>,
    /// Part-global validity bitset, LSB-first words.
    validity: Vec<u64>,
    has_null: bool,
    rows: u64,
    nonnull: u64,
    value_bytes: u64,
    oversize_values: u64,
    /// Canonical payload bytes of oversize values only (SB-10 grain pricing:
    /// oversize varlenas route to the overflow stream at encode, so the
    /// granule byte bound prices them at their 16-B inline OverflowRef stub,
    /// not their payload — this accumulator subtracts them out).
    oversize_bytes: u64,
    constant: bool,
    first_canon: Option<Vec<u8>>,
    hash: LogicalColHash,
    /// D2 side channel (None = no inherited structure; the rebuild path).
    dict_side: Option<DictSide>,
    /// PGRC-WRITER-FAST `bank`: the non-null rows of a varlena column are
    /// NOT folded into `hash` at append — the seal folds them once per
    /// distinct value from the dict build's counted entries, inside the
    /// meta hash lane, or by one walk ([`Self::logical_hash`]). Nulls
    /// still fold at append.
    bank_deferred: bool,
    /// PGRC-WRITER-FAST `run`: the last word value's (masked word, lo, hi)
    /// digests.
    run_last: Option<(u64, u64, u64)>,
    /// Every non-null payload was validated as UTF-8 before it was appended
    /// (a parquet string column the reader checked at page decode): the
    /// seal's text-semantics verdict needs no walk.
    utf8_proven: bool,
}

impl ColBuffer {
    /// An uncharged buffer (crate tests, tools): its blocks stay on the allocating thread's account. A load builds
    /// its buffers with [`ColBuffer::with_owner`].
    pub fn new(schema: ColSchema) -> ColBuffer {
        ColBuffer::with_owner(schema, None)
    }

    /// CONVERT W3-2b: a buffer whose blocks are charged to `owner` (the loading statement, passed down from the COPY
    /// leader), kind `Region`; `None`: uncharged, as [`ColBuffer::new`].
    pub fn with_owner(schema: ColSchema, owner: Option<memledger::Owner>) -> ColBuffer {
        ColBuffer {
            charge: HeldCharge::new(owner),
            schema,
            words: Vec::new(),
            heap: Vec::new(),
            validity: Vec::new(),
            has_null: false,
            rows: 0,
            nonnull: 0,
            value_bytes: 0,
            oversize_values: 0,
            oversize_bytes: 0,
            constant: true,
            first_canon: None,
            hash: LogicalColHash::new(),
            dict_side: None,
            bank_deferred: schema.class == StorageClass::VarlenaVerbatim,
            run_last: None,
            utf8_proven: false,
        }
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    pub fn has_null(&self) -> bool {
        self.has_null
    }

    /// Record that every payload appended to this (chunk) buffer is valid
    /// UTF-8. The caller's contract; varlena columns only.
    pub fn set_utf8_proven(&mut self) {
        self.utf8_proven = self.schema.class == StorageClass::VarlenaVerbatim;
    }

    pub fn utf8_proven(&self) -> bool {
        self.utf8_proven
    }

    /// Approximate resident bytes (the part-cut byte budget input).
    pub fn approx_bytes(&self) -> u64 {
        (self.words.len() * 8 + self.heap.len() + self.validity.len() * 8) as u64
    }

    /// D3343: the heap bytes this buffer holds (its capacity, side channel included).
    pub fn held_bytes(&self) -> u64 {
        let side = self.dict_side.as_ref().map_or(0, |d| d.rows.capacity() * 8);
        (self.words.capacity() * 8 + self.heap.capacity() + self.validity.capacity() * 8 + side) as u64
    }

    /// CONVERT W3-2b: the owner this buffer's blocks are charged to (`None`: uncharged). What the seal derives from
    /// the part (its sort, its lanes, its dictionary, its image) takes the same owner.
    pub fn owner(&self) -> Option<memledger::Owner> {
        self.charge.owner()
    }

    /// CONVERT W3-2b: the charge brought to [`Self::held_bytes`] now, on this thread. The bulk faces (reserves,
    /// splice, lanes, permute, the side channel) call it themselves; the serial writer's per-row appends are synced
    /// by the writer once per cut granule (`TableWriter::append_row`), so they lag at most one granule's growth.
    /// CONVERT W3-2b: the buffer's bytes moved to a charge on `owner` (`None`: uncharged); the old charge gives its
    /// bytes back first. The writer sets its owner before its first row.
    pub fn set_owner(&mut self, owner: Option<memledger::Owner>) {
        self.charge = HeldCharge::new(owner);
        self.sync_charge();
    }

    #[inline]
    pub fn sync_charge(&mut self) {
        self.charge.sync(self.held_bytes());
    }

    /// Heap bytes buffered so far — split out of [`ColBuffer::approx_bytes`]
    /// for the parallel cut cursor (#597). The heap is the ONE
    /// `approx_bytes` component whose part-accumulated length is not the
    /// sum of chunk-local lengths: [`ColBuffer::splice_chunk`] pads the
    /// accumulated heap to 8 at each interior chunk seam, exactly as serial
    /// ingest pads before the seam's first entry. The cursor therefore
    /// accumulates each column's heap through that same pad-to-8-then-add
    /// arithmetic (words and validity, being seam-invariant, sum flat).
    pub fn heap_len(&self) -> u64 {
        self.heap.len() as u64
    }

    /// The O-10 logical column identity of the rows appended so far: the
    /// append-time fold, plus — when the non-null fold is deferred
    /// ([`Self::bank_deferred`]) — one walk over the non-null payloads.
    pub fn logical_hash(&self) -> WriteResult<LogicalColHash> {
        let mut h = self.hash;
        if self.bank_deferred {
            for row in 0..self.rows {
                if let Some(p) = self.varlena_payload(row)? {
                    h.observe(p);
                }
            }
        }
        Ok(h)
    }

    /// PGRC-WRITER-FAST `alloc`: reserve an empty chunk buffer for `rows`
    /// rows carrying `heap_payload` payload bytes (+ per-entry header / pad
    /// slack) — the normalize face; capacity only.
    pub fn reserve_rows(&mut self, rows: usize, heap_payload: usize) -> WriteResult<()> {
        pgrc_format::try_reserve_more(&mut self.words, rows, "ingest")?; // alloc: Exec charged self.charge
        pgrc_format::try_reserve_more(&mut self.validity, rows.div_ceil(64) + 1, "ingest")?; // alloc: Exec charged self.charge
        if matches!(self.schema.class, StorageClass::Fixed { .. } | StorageClass::VarlenaVerbatim) {
            pgrc_format::try_reserve_more(&mut self.heap, heap_payload + rows * 12 + 8, "ingest")?; // alloc: Exec charged self.charge
        }
        self.sync_charge();
        Ok(())
    }

    /// PGRC-WRITER-FAST `alloc`: reserve this (empty) accumulator for
    /// splicing `chunks` (exact word / validity totals, heap + one 8-B pad
    /// per seam) so the splice never regrows — capacity only, bytes cannot
    /// move.
    pub fn reserve_splice<'c>(&mut self, chunks: impl Iterator<Item = &'c ColBuffer>) -> WriteResult<()> {
        let (mut w, mut v, mut h) = (0usize, 0usize, 0usize);
        for c in chunks {
            w += c.words.len();
            v += c.validity.len();
            h += c.heap.len() + 8;
        }
        pgrc_format::try_reserve_more(&mut self.words, w, "ingest")?; // alloc: Exec charged self.charge
        pgrc_format::try_reserve_more(&mut self.validity, v, "ingest")?; // alloc: Exec charged self.charge
        if matches!(self.schema.class, StorageClass::Fixed { .. } | StorageClass::VarlenaVerbatim) {
            pgrc_format::try_reserve_more(&mut self.heap, h, "ingest")?; // alloc: Exec charged self.charge
        }
        self.sync_charge();
        Ok(())
    }

    /// PGRC-WRITER-FAST `bank`: is the non-null part of the logical hash
    /// deferred to the seal? (Varlena columns.)
    pub fn bank_deferred(&self) -> bool {
        self.bank_deferred
    }

    /// The append-time fold alone: the whole identity when nothing is
    /// deferred, the null rows' part when [`Self::bank_deferred`] (the
    /// seal adds the non-null part).
    pub fn eager_hash(&self) -> &LogicalColHash {
        &self.hash
    }

    /// Attach the D2 side channel for rows appended so far. `rows` must be
    /// row-dense over the CURRENT row count (attach happens after a chunk's
    /// appends, before any splice into a part accumulator).
    pub fn attach_dict_side(
        &mut self,
        sources: Vec<std::sync::Arc<dyn DictEntries>>,
        rows: Vec<u64>,
    ) -> WriteResult<()> {
        if rows.len() as u64 != self.rows {
            return Err(WriteError::Contract {
                detail: "dict side channel not row-dense",
            });
        }
        self.dict_side = Some(DictSide { sources, rows });
        self.sync_charge();
        Ok(())
    }

    /// The D2 side channel, iff it covers EVERY row of the part (the
    /// coverage law — partial coverage is dropped at splice time).
    pub fn dict_side(&self) -> Option<&DictSide> {
        match &self.dict_side {
            Some(s) if s.rows.len() as u64 == self.rows => Some(s),
            _ => None,
        }
    }

    /// Room for `rows` more rows and `heap_bytes` more image bytes (C1-REG): a pool worker then appends a chunk
    /// without growing the buffers, so they stay allocations of the thread that owns (and frees) the column.
    pub fn reserve(&mut self, rows: u64, heap_bytes: u64) -> WriteResult<()> {
        pgrc_format::try_reserve_more(&mut self.words, rows as usize, "ingest")?;
        let words = ((self.rows + rows).div_ceil(64) as usize).saturating_sub(self.validity.len());
        pgrc_format::try_reserve_more(&mut self.validity, words, "ingest")?;
        pgrc_format::try_reserve_more(&mut self.heap, heap_bytes as usize, "ingest")?;
        Ok(())
    }

    /// The buffers' capacities (words, validity, heap).
    #[cfg(test)]
    pub(crate) fn capacities(&self) -> [usize; 3] {
        [self.words.capacity(), self.validity.capacity(), self.heap.capacity()]
    }

    fn push_validity(&mut self, valid: bool) {
        let bit = self.rows as usize;
        let word = bit / 64;
        if word >= self.validity.len() {
            self.validity.push(0); // alloc: Exec charged self.charge
        }
        if valid {
            self.validity[word] |= 1 << (bit % 64);
        } else {
            self.has_null = true;
        }
    }

    fn note_canon(&mut self, canon: &[u8]) {
        if !self.bank_deferred {
            self.hash.observe(canon);
        }
        self.value_bytes += canon.len() as u64;
        match &self.first_canon {
            None => self.first_canon = Some(canon.to_vec()),
            Some(first) => {
                if self.constant && first.as_slice() != canon {
                    self.constant = false;
                }
            }
        }
    }

    pub fn append_null(&mut self) {
        self.push_validity(false);
        self.words.push(0); // alloc: Exec charged self.charge
        self.hash.observe_null();
        self.rows += 1;
    }

    /// Byval word (datum extension convention per spec §6.7). Bool datums
    /// normalize to 0/1.
    pub fn append_word(&mut self, w: u64) -> WriteResult<()> {
        let w = match self.schema.class {
            StorageClass::Bool => (w != 0) as u64,
            StorageClass::ByvalWord { .. } | StorageClass::F32 | StorageClass::F64 => w,
            _ => {
                return Err(WriteError::Contract {
                    detail: "Word datum on byref class",
                })
            }
        };
        self.push_validity(true);
        let width = word_canonical_width(self.schema.class);
        // The same facts as `note_canon(&w.to_le_bytes()[..width])`, in the
        // one-word form (PGRC-WRITER-FAST `word`): the digests from the
        // masked word (a repeat of the previous value re-adds its digests —
        // `run`), the constancy check on the masked word.
        let mask = if width >= 8 { u64::MAX } else { (1u64 << (8 * width)) - 1 };
        let wm = w & mask;
        match self.run_last {
            Some((pw, lo, hi)) if pw == wm => self.hash.add(lo, hi),
            _ => {
                let (wm, lo, hi) = LogicalColHash::word_digests(w, width);
                self.hash.add(lo, hi);
                self.run_last = Some((wm, lo, hi));
            }
        }
        self.value_bytes += width as u64;
        match &self.first_canon {
            None => self.first_canon = Some(pgrc_format::try_to_vec(&w.to_le_bytes()[..width], "ingest")?),
            Some(first) => {
                if self.constant {
                    let mut f = [0u8; 8];
                    f[..width].copy_from_slice(&first[..width]);
                    if u64::from_le_bytes(f) != wm {
                        self.constant = false;
                    }
                }
            }
        }
        self.words.push(w); // alloc: Exec charged self.charge
        self.nonnull += 1;
        self.rows += 1;
        Ok(())
    }

    /// Fixed(N) image, exactly N bytes.
    pub fn append_fixed(&mut self, image: &[u8]) -> WriteResult<()> {
        let StorageClass::Fixed { len } = self.schema.class else {
            return Err(WriteError::Contract {
                detail: "fixed append on non-fixed class",
            });
        };
        if image.len() != len as usize {
            return Err(WriteError::Contract {
                detail: "fixed image length mismatch",
            });
        }
        self.push_validity(true);
        pad8(&mut self.heap);
        let off = self.heap.len() as u64;
        self.heap.extend_from_slice(image); // alloc: Exec charged self.charge
        self.note_canon(image);
        self.words.push(off); // alloc: Exec charged self.charge
        self.nonnull += 1;
        self.rows += 1;
        Ok(())
    }

    /// Normalized varlena PAYLOAD bytes (header re-added here so the stored
    /// heap entry is varlena-shaped — StrView §7b).
    pub fn append_varlena_payload(&mut self, payload: &[u8]) -> WriteResult<()> {
        if self.schema.class != StorageClass::VarlenaVerbatim {
            return Err(WriteError::Contract {
                detail: "varlena append on non-varlena class",
            });
        }
        self.push_validity(true);
        pad8(&mut self.heap);
        let off = self.heap.len() as u64;
        self.heap
            .extend_from_slice(&varlena_header_4b_u(payload.len() as u32).to_le_bytes()); // alloc: Exec charged self.charge
        self.heap.extend_from_slice(payload); // alloc: Exec charged self.charge
        self.note_canon(payload);
        if payload.len() as u32 >= OVERSIZE_THRESHOLD {
            self.oversize_values += 1;
            self.oversize_bytes += payload.len() as u64;
        }
        self.words.push(off); // alloc: Exec charged self.charge
        self.nonnull += 1;
        self.rows += 1;
        Ok(())
    }

    /// FASTADD (pgrc-plans/FASTADD.done): `n` rows of one value (a column's `attmissingval`, or NULL), as `n` appends
    /// of it would leave the buffer, except that a by-reference value's image is stored once and every row's word
    /// addresses it (a stand-in part's column of a long default stays one image, not `n`). A varlena in any inline
    /// form normalizes as [`normalize_varlena`] does; an external pointer refuses.
    pub fn append_repeated(&mut self, d: RawDatum<'_>, n: u64) -> WriteResult<()> {
        self.reserve(n, 0)?;
        match (d, self.schema.class) {
            (RawDatum::Null, _) => (0..n).for_each(|_| self.append_null()),
            (RawDatum::Word(w), StorageClass::ByvalWord { .. } | StorageClass::F32 | StorageClass::F64 | StorageClass::Bool) => {
                for _ in 0..n {
                    self.append_word(w)?;
                }
            }
            (RawDatum::Bytes(b), StorageClass::Fixed { len }) => {
                if b.len() != len as usize {
                    return Err(WriteError::Contract { detail: "fixed image length mismatch" });
                }
                pad8(&mut self.heap);
                let off = self.heap.len() as u64;
                self.heap.extend_from_slice(b);
                self.repeat_image(off, b, n, false);
            }
            (RawDatum::Bytes(b), StorageClass::VarlenaVerbatim) => {
                let mut scratch = Vec::new(); // alloc: Cold
                let (payload, _) = normalize_varlena(b, &mut NoExternalDetoast, &mut scratch)?;
                pad8(&mut self.heap);
                let off = self.heap.len() as u64;
                self.heap.extend_from_slice(&varlena_header_4b_u(payload.len() as u32).to_le_bytes());
                self.heap.extend_from_slice(payload);
                self.repeat_image(off, payload, n, true);
            }
            _ => return Err(WriteError::Contract { detail: "datum shape does not match column class" }),
        }
        Ok(())
    }

    /// `n` non-NULL rows whose words address the image at heap offset `off` (`canon`: its canonical bytes).
    fn repeat_image(&mut self, off: u64, canon: &[u8], n: u64, varlena: bool) {
        for _ in 0..n {
            self.push_validity(true);
            self.note_canon(canon);
            if varlena && canon.len() as u32 >= OVERSIZE_THRESHOLD {
                self.oversize_values += 1;
                self.oversize_bytes += canon.len() as u64;
            }
            self.words.push(off);
            self.nonnull += 1;
            self.rows += 1;
        }
    }

    /// Column-at-a-time [`Self::append_null`] / [`Self::append_word`] over a
    /// run of rows (`nulls[i]` = row i is NULL; its word is ignored). The
    /// resulting state is bit-identical to the per-row calls in row order.
    pub fn append_word_lane(&mut self, words: &[u64], nulls: &[bool]) -> WriteResult<()> {
        let r = self.word_lane(words, nulls);
        self.sync_charge();
        r
    }

    fn word_lane(&mut self, words: &[u64], nulls: &[bool]) -> WriteResult<()> {
        let n = words.len();
        if nulls.len() != n {
            return Err(WriteError::Contract {
                detail: "word lane length mismatch",
            });
        }
        let byval = matches!(
            self.schema.class,
            StorageClass::ByvalWord { .. } | StorageClass::F32 | StorageClass::F64 | StorageClass::Bool
        );
        if !byval {
            for (i, &nl) in nulls.iter().enumerate() {
                if nl {
                    self.append_null();
                } else {
                    self.append_word(words[i])?;
                }
            }
            return Ok(());
        }
        let is_bool = self.schema.class == StorageClass::Bool;
        let width = word_canonical_width(self.schema.class);
        let mask = if width >= 8 { u64::MAX } else { (1u64 << (8 * width)) - 1 };
        let r0 = self.rows as usize;
        let nnull = nulls.iter().filter(|&&nl| nl).count();
        pgrc_format::try_reserve_more(&mut self.words, n, "ingest")?; // alloc: Exec charged self.charge
        let wbase = self.words.len();
        if is_bool {
            self.words.extend(words.iter().map(|&w| (w != 0) as u64)); // alloc: Exec charged self.charge
        } else {
            self.words.extend_from_slice(words); // alloc: Exec charged self.charge
        }
        let need_v = (r0 + n).div_ceil(64);
        if need_v > self.validity.len() {
            pgrc_format::try_resize(&mut self.validity, need_v, 0, "ingest")?; // alloc: Exec charged self.charge
        }
        if nnull == 0 {
            set_bits(&mut self.validity, r0, r0 + n);
        } else {
            for ((d, &nl), r) in self.words[wbase..].iter_mut().zip(nulls).zip(r0..) {
                if nl {
                    *d = 0;
                } else {
                    self.validity[r >> 6] |= 1 << (r & 63);
                }
            }
        }
        // Branchless digests: every non-null row adds its two word digests
        // (5 multiplies, independent across rows), so no run cache branch
        // mispredicts on high-cardinality columns; the sum is the per-row
        // appends' sum exactly (a cached digest IS the recomputed one).
        // Constancy is one xor-or against the first value.
        let mut h = LogicalColHash::new();
        let first: Option<u64> = self.first_canon.as_ref().map(|f| {
            let mut b = [0u8; 8];
            b[..width].copy_from_slice(&f[..width]);
            u64::from_le_bytes(b)
        });
        let added = &self.words[wbase..];
        let first_new = if nnull == 0 {
            added.first().copied()
        } else {
            added.iter().zip(nulls).find(|(_, &nl)| !nl).map(|(&w, _)| w)
        };
        let f0 = first.or(first_new.map(|w| w & mask)).unwrap_or(0);
        let mut diff = 0u64;
        if nnull == 0 {
            // Granules of WORD_GRANULE rows: a granule whose words are all
            // equal (most of a key-ordered block: the sort key's neighbours
            // repeat) folds ONE digest times its length, the previous
            // granule's digest reused when the value repeats; the others
            // fold per row. The sum is the per-row sum exactly.
            let mut cached: Option<(u64, u64, u64)> = None;
            let mut gs = added.chunks_exact(WORD_GRANULE);
            for g in gs.by_ref() {
                let v = g[0];
                if g.iter().fold(0u64, |a, &w| a | (w ^ v)) & mask == 0 {
                    let d = match cached {
                        Some(d) if d.0 == v & mask => d,
                        _ => LogicalColHash::word_digests(v, width),
                    };
                    h.observe_repeated(d.1, d.2, WORD_GRANULE as u64);
                    diff |= d.0 ^ f0;
                    cached = Some(d);
                } else {
                    for &w in g {
                        let (wm, lo, hi) = LogicalColHash::word_digests(w, width);
                        h.add(lo, hi);
                        diff |= wm ^ f0;
                    }
                }
            }
            for &w in gs.remainder() {
                let (wm, lo, hi) = LogicalColHash::word_digests(w, width);
                h.add(lo, hi);
                diff |= wm ^ f0;
            }
        } else {
            for (&w, &nl) in added.iter().zip(nulls) {
                if !nl {
                    let (wm, lo, hi) = LogicalColHash::word_digests(w, width);
                    h.add(lo, hi);
                    diff |= wm ^ f0;
                }
            }
        }
        let constant = self.constant & (diff == 0);
        let last_new = if nnull == 0 {
            added.last().copied()
        } else {
            added.iter().zip(nulls).rev().find(|(_, &nl)| !nl).map(|(&w, _)| w)
        };
        let run_last = match last_new {
            Some(w) => Some(LogicalColHash::word_digests(w, width)),
            None => self.run_last,
        };
        let first_w = if first.is_none() { first_new } else { None };
        for _ in 0..nnull {
            h.observe_null();
        }
        if let Some(w) = first_w {
            self.first_canon = Some(pgrc_format::try_to_vec(&w.to_le_bytes()[..width], "ingest")?);
        }
        self.hash.merge(&h);
        self.has_null |= nnull != 0;
        self.run_last = run_last;
        self.constant = constant;
        self.rows += n as u64;
        let nonnull = (n - nnull) as u64;
        self.nonnull += nonnull;
        self.value_bytes += nonnull * width as u64;
        Ok(())
    }

    /// Column-at-a-time appends of raw varlena images (`spans` index
    /// `arena`; NULL rows ignore theirs): [`normalize_varlena`] +
    /// [`Self::append_varlena_payload`] per row, with the heap reserved once.
    /// Fixed(N) columns take their images through [`Self::append_fixed`].
    pub fn append_image_lane(
        &mut self,
        arena: &[u8],
        spans: &[(u32, u32)],
        nulls: &[bool],
        ext: &mut dyn ExternalDetoast,
        scratch: &mut Vec<u8>,
    ) -> WriteResult<()> {
        let r = self.image_lane(arena, spans, nulls, ext, scratch);
        self.sync_charge();
        r
    }

    fn image_lane(
        &mut self,
        arena: &[u8],
        spans: &[(u32, u32)],
        nulls: &[bool],
        ext: &mut dyn ExternalDetoast,
        scratch: &mut Vec<u8>,
    ) -> WriteResult<()> {
        let n = spans.len();
        if nulls.len() != n {
            return Err(WriteError::Contract {
                detail: "image lane length mismatch",
            });
        }
        let mut payload_bytes = 0usize;
        for (i, &(_, len)) in spans.iter().enumerate() {
            if !nulls[i] {
                payload_bytes += len as usize;
            }
        }
        let need_v = (self.rows as usize + n).div_ceil(64);
        pgrc_format::try_reserve_more(&mut self.words, n, "ingest")?; // alloc: Exec charged self.charge
        let more_v = need_v.saturating_sub(self.validity.len());
        pgrc_format::try_reserve_more(&mut self.validity, more_v, "ingest")?; // alloc: Exec charged self.charge
        pgrc_format::try_reserve_more(&mut self.heap, payload_bytes + n * 12 + 8, "ingest")?; // alloc: Exec charged self.charge
        let fixed = matches!(self.schema.class, StorageClass::Fixed { .. });
        if self.schema.class == StorageClass::VarlenaVerbatim && self.bank_deferred {
            return self.append_varlena_images(arena, spans, nulls, ext, scratch);
        }
        for (i, &(off, len)) in spans.iter().enumerate() {
            if nulls[i] {
                self.append_null();
                continue;
            }
            let image = arena.get(off as usize..off as usize + len as usize).ok_or(WriteError::Contract {
                detail: "image span out of bounds",
            })?;
            if fixed {
                self.append_fixed(image)?;
            } else if let Some(payload) = plain_4b_payload(image) {
                self.append_varlena_payload(payload)?;
            } else {
                let (payload, _) = normalize_varlena(image, ext, scratch)?;
                self.append_varlena_payload(payload)?;
            }
        }
        Ok(())
    }

    /// [`Self::append_image_lane`]'s varlena loop: per row exactly
    /// [`Self::append_null`] or [`normalize_varlena`] +
    /// [`Self::append_varlena_payload`] (the non-null fold deferred to the
    /// seal), with the buffer's fields held in locals.
    fn append_varlena_images(
        &mut self,
        arena: &[u8],
        spans: &[(u32, u32)],
        nulls: &[bool],
        ext: &mut dyn ExternalDetoast,
        scratch: &mut Vec<u8>,
    ) -> WriteResult<()> {
        let n = spans.len();
        let r0 = self.rows as usize;
        let need_v = (r0 + n).div_ceil(64);
        if need_v > self.validity.len() {
            pgrc_format::try_resize(&mut self.validity, need_v, 0, "ingest")?; // alloc: Exec charged self.charge
        }
        let Self {
            words,
            heap,
            validity,
            first_canon,
            constant,
            ..
        } = self;
        let mut h = LogicalColHash::new();
        let (mut nnull, mut value_bytes, mut ov_n, mut ov_b) = (0u64, 0u64, 0u64, 0u64);
        for (i, (&(off, len), &nl)) in spans.iter().zip(nulls).enumerate() {
            if nl {
                words.push(0); // alloc: Exec charged self.charge
                h.observe_null();
                nnull += 1;
                continue;
            }
            let image = arena.get(off as usize..off as usize + len as usize).ok_or(WriteError::Contract {
                detail: "image span out of bounds",
            })?;
            let r = r0 + i;
            validity[r >> 6] |= 1 << (r & 63);
            pad8(heap);
            let at = heap.len() as u64;
            // A plain 4B-U image IS the heap entry (header + payload): one copy.
            let payload = match plain_4b_payload(image) {
                Some(p) => {
                    heap.extend_from_slice(&image[..4 + p.len()]); // alloc: Exec charged self.charge
                    p
                }
                None => {
                    let p = normalize_varlena(image, ext, scratch)?.0;
                    heap.extend_from_slice(&varlena_header_4b_u(p.len() as u32).to_le_bytes()); // alloc: Exec charged self.charge
                    heap.extend_from_slice(p); // alloc: Exec charged self.charge
                    p
                }
            };
            value_bytes += payload.len() as u64;
            match first_canon {
                None => *first_canon = Some(pgrc_format::try_to_vec(payload, "ingest")?),
                Some(f) => {
                    if *constant && f.as_slice() != payload {
                        *constant = false;
                    }
                }
            }
            if payload.len() as u32 >= OVERSIZE_THRESHOLD {
                ov_n += 1;
                ov_b += payload.len() as u64;
            }
            words.push(at); // alloc: Exec charged self.charge
        }
        self.hash.merge(&h);
        self.has_null |= nnull != 0;
        self.rows += n as u64;
        self.nonnull += n as u64 - nnull;
        self.value_bytes += value_bytes;
        self.oversize_values += ov_n;
        self.oversize_bytes += ov_b;
        Ok(())
    }

    /// Exact whole-part election stats (analyze-then-elect input).
    pub fn stream_stats(&self) -> crate::elect::StreamStats {
        crate::elect::StreamStats {
            class: self.schema.class,
            rows: self.rows,
            nonnull: self.nonnull,
            constant: self.constant,
            value_bytes: self.value_bytes,
            oversize_values: self.oversize_values,
        }
    }

    /// The SB-10 grain-election byte operand for this stream: exact
    /// whole-part canonical value bytes with every OVERSIZE value priced at
    /// its 16-B inline OverflowRef stub (the existing overflow law — those
    /// payloads land in the overflow stream, not the granule).
    pub fn granule_pricing_bytes(&self) -> u64 {
        self.value_bytes - self.oversize_bytes + self.oversize_values * 16
    }

    /// The WHOLE part's row-dense datum currency in ONE materialization
    /// (SEAL-FUSION): byval classes borrow the staged words outright (the
    /// datum word array IS the row-dense currency — zero copy, the old
    /// per-granule copy walk disappears); byref classes materialize heap
    /// pointers in a single pass. Granule inputs at EVERY grain are slices
    /// of this one array (granule g at grain G covers rows
    /// `[g·G, g·G + rows_in_granule)`), which is byte-for-byte what
    /// [`ColBuffer::granule_ptrs`] materialized per granule — the elected-
    /// grain and default-grain slicings both cover the identical sequence,
    /// so the seal's double materialization (#1b) is structural history.
    pub fn part_ptrs(&self) -> std::borrow::Cow<'_, [u64]> {
        let byref = matches!(
            self.schema.class,
            StorageClass::Fixed { .. } | StorageClass::VarlenaVerbatim
        );
        if !byref {
            return std::borrow::Cow::Borrowed(&self.words);
        }
        let mut out: Vec<u64> = Vec::with_capacity(self.rows as usize); // alloc: Exec charged the caller's ptrs_charge (seal_part, Cow::Owned)
        let base = self.heap.as_ptr() as u64;
        for r in 0..self.rows as usize {
            let w = self.words[r];
            if self.is_valid(r) {
                out.push(base + w);
            } else {
                out.push(w);
            }
        }
        std::borrow::Cow::Owned(out)
    }

    /// Materialize granule `g`'s datum pointers into `ptrs` and return the
    /// `EncodeInput` facts. Granule slicing honors the part's elected
    /// `grain` (SB-10). Byref pointers reference `self.heap`, which is
    /// stable for the borrow's lifetime (no appends during seal).
    pub fn granule_ptrs(&self, g: u32, grain: GranuleGrain, ptrs: &mut Vec<u64>) -> u32 {
        let start = g as u64 * grain.rows() as u64;
        let rows_g = pgrc_format::geom::rows_in_granule_at(self.rows, grain, g);
        ptrs.clear();
        let byref = matches!(
            self.schema.class,
            StorageClass::Fixed { .. } | StorageClass::VarlenaVerbatim
        );
        for r in 0..rows_g as u64 {
            let w = self.words[(start + r) as usize];
            if byref && self.is_valid((start + r) as usize) {
                ptrs.push(self.heap.as_ptr() as u64 + w);
            } else {
                ptrs.push(w);
            }
        }
        rows_g
    }

    fn is_valid(&self, row: usize) -> bool {
        let word = row / 64;
        word < self.validity.len() && (self.validity[word] >> (row % 64)) & 1 == 1
    }

    /// Row validity (cluster-sort comparator input; IN-1).
    pub fn valid_at(&self, row: u64) -> bool {
        self.is_valid(row as usize)
    }

    /// Row `row`'s datum word (byval classes: the value in extension
    /// convention; byref classes: the heap offset). Comparator input.
    pub fn word_at(&self, row: u64) -> u64 {
        self.words[row as usize]
    }

    /// Row `row`'s Fixed(N) image bytes (None = null).
    pub fn fixed_at(&self, row: u64) -> WriteResult<Option<&[u8]>> {
        let StorageClass::Fixed { len } = self.schema.class else {
            return Err(WriteError::Contract {
                detail: "fixed_at on non-fixed class",
            });
        };
        if !self.is_valid(row as usize) {
            return Ok(None);
        }
        let off = self.words[row as usize] as usize;
        Ok(Some(&self.heap[off..off + len as usize]))
    }

    /// Rebuild this buffer with rows in `perm` order (the cluster-sort
    /// apply; IN-1). `perm` must be a permutation of `0..rows`; the rebuild
    /// routes every row through the SAME `append_*` calls ingest used, so
    /// the permuted buffer is bit-identical to having ingested the rows in
    /// that order (heap pad-8 placement included). The O-10 logical
    /// multiset hash is order-independent by construction, so the digest is
    /// unchanged — the sort is byte-visible on disk and invisible to the
    /// logical identity, exactly as a sort must be.
    pub fn permuted(&self, perm: &[u32]) -> WriteResult<ColBuffer> {
        if perm.len() as u64 != self.rows {
            return Err(WriteError::Contract {
                detail: "permutation length != row count",
            });
        }
        let mut out = ColBuffer::with_owner(self.schema, self.owner());
        for &r in perm {
            let r = r as u64;
            if r >= self.rows {
                return Err(WriteError::Contract {
                    detail: "permutation index out of range",
                });
            }
            if !self.is_valid(r as usize) {
                out.append_null();
                continue;
            }
            match self.schema.class {
                StorageClass::ByvalWord { .. }
                | StorageClass::F32
                | StorageClass::F64
                | StorageClass::Bool => out.append_word(self.words[r as usize])?,
                StorageClass::Fixed { .. } => {
                    let img = self.fixed_at(r)?.expect("valid row");
                    out.append_fixed(img)?
                }
                StorageClass::VarlenaVerbatim => {
                    let payload = self.varlena_payload(r)?.expect("valid row");
                    out.append_varlena_payload(payload)?
                }
            }
        }
        out.utf8_proven = self.utf8_proven;
        // D2 side channel follows the permutation (row-dense slots move
        // with their rows; the source table is order-independent).
        if let Some(side) = self.dict_side() {
            let rows: Vec<u64> = perm.iter().map(|&r| side.rows[r as usize]).collect(); // alloc: Exec charged out.charge
            out.attach_dict_side(side.sources.clone(), rows)?;
        }
        out.sync_charge();
        Ok(out)
    }

    /// Upper bound on the decode-arena bytes granule `g` needs at verify
    /// (payload bytes + per-value header/alignment overhead), at the part's
    /// elected `grain`.
    pub fn granule_arena_bound(&self, g: u32, grain: GranuleGrain) -> u64 {
        let rows_g = pgrc_format::geom::rows_in_granule_at(self.rows, grain, g) as u64;
        match self.schema.class {
            StorageClass::ByvalWord { .. }
            | StorageClass::F32
            | StorageClass::F64
            | StorageClass::Bool => 0,
            StorageClass::Fixed { len } => rows_g * (len as u64 + 8),
            StorageClass::VarlenaVerbatim => {
                let start = g as u64 * grain.rows() as u64;
                // Null slots still cost arena on pointer classes (decoders
                // emit the canonical zero-length placeholder entries) and
                // images align per value — mirror the codec crate's verify
                // envelope (`worst_granule_arena`), never undercount. The
                // old bound skipped null slots entirely and undercounted
                // alignment; the shared grow-only verify arena masked it
                // until a null-carrying single-column seal hit
                // `ArenaExhausted` (found RED by the CMP-A wrapped-part
                // null fixture).
                let mut sum = rows_g * 16;
                for r in start..start + rows_g {
                    if self.is_valid(r as usize) {
                        let off = self.words[r as usize] as usize;
                        let header =
                            u32::from_le_bytes(self.heap[off..off + 4].try_into().expect("len 4"));
                        // `header >> 2` = payload + the 4-byte header.
                        sum += ((header >> 2) as u64).div_ceil(8) * 8 + 8;
                    }
                }
                sum
            }
        }
    }

    /// Row `row`'s stored varlena PAYLOAD bytes (None = null). The shred
    /// lane derivation reads parent documents through this.
    pub fn varlena_payload(&self, row: u64) -> WriteResult<Option<&[u8]>> {
        if self.schema.class != StorageClass::VarlenaVerbatim {
            return Err(WriteError::Contract {
                detail: "varlena_payload on non-varlena class",
            });
        }
        if row >= self.rows {
            return Err(WriteError::Contract {
                detail: "varlena_payload row out of range",
            });
        }
        if !self.is_valid(row as usize) {
            return Ok(None);
        }
        let off = self.words[row as usize] as usize;
        let (_, payload) =
            pgrc_format::wire::varlena_entry_at(&self.heap, off, "col heap entry")
                .map_err(WriteError::Format)?;
        Ok(Some(payload))
    }

    /// Rows `lo..hi`'s payload byte lengths into `lens` (0 at a NULL row)
    /// and their nulls into `nulls`, for a column-at-a-time consumer (the
    /// octet lane): [`Self::varlena_payload`]'s checks, one tight walk.
    pub fn varlena_payload_lens(
        &self,
        lo: u64,
        hi: u64,
        lens: &mut Vec<u64>,
        nulls: &mut Vec<bool>,
    ) -> WriteResult<()> {
        if self.schema.class != StorageClass::VarlenaVerbatim {
            return Err(WriteError::Contract {
                detail: "varlena_payload on non-varlena class",
            });
        }
        if hi > self.rows || lo > hi {
            return Err(WriteError::Contract {
                detail: "varlena_payload row out of range",
            });
        }
        let n = (hi - lo) as usize;
        pgrc_format::try_reserve_more(lens, n, "ingest")?;
        pgrc_format::try_reserve_more(nulls, n, "ingest")?;
        for r in lo as usize..hi as usize {
            if !self.is_valid(r) {
                lens.push(0);
                nulls.push(true);
                continue;
            }
            let off = self.words[r] as usize;
            let head: [u8; 4] = self
                .heap
                .get(off..off + 4)
                .and_then(|h| h.try_into().ok())
                .ok_or(WriteError::Format(pgrc_format::FormatError::Bounds { at: "col heap entry" }))?;
            let len = pgrc_format::wire::varlena_4b_u_payload_len(u32::from_le_bytes(head), "col heap entry")
                .map_err(WriteError::Format)? as usize;
            if off + 4 + len > self.heap.len() {
                return Err(WriteError::Format(pgrc_format::FormatError::Bounds { at: "col heap entry" }));
            }
            lens.push(len as u64);
            nulls.push(false);
        }
        Ok(())
    }

    /// Granule `g`'s validity words at the part's elected `grain` — a
    /// word-aligned slice: every ladder grain is a multiple of 1024, so
    /// `grain / 64` is whole (128 at the default 8192 grain).
    pub fn granule_validity(&self, g: u32, grain: GranuleGrain) -> Option<&[u64]> {
        if !self.has_null {
            return None;
        }
        let words_per_granule = (grain.rows() / 64) as usize;
        let rows_g = pgrc_format::geom::rows_in_granule_at(self.rows, grain, g) as usize;
        let start = g as usize * words_per_granule;
        let words = rows_g.div_ceil(64);
        Some(&self.validity[start..(start + words).min(self.validity.len())])
    }

    /// Splice a chunk-local buffer onto the end of `self` — THE
    /// seam-replayed-tracker mechanism (chunk M3-I; charter
    /// `pgrcolumnar-v2.md` §1 "ordered-commit parallel COPY with
    /// seam-replayed trackers").
    ///
    /// Parallel ingest workers accumulate `chunk`s independently (each built
    /// from row 0 by the same `append_*` calls); the part assembler replays
    /// them IN INPUT ORDER through this splice, which reproduces EXACTLY the
    /// state serial appends would have produced — field by field:
    ///
    /// - `heap`: 8-aligned base + verbatim chunk bytes ≡ serial `pad8`
    ///   placement (entries are padded BEFORE each append, never after, so a
    ///   chunk's relative layout transplants verbatim onto any 8-aligned
    ///   base);
    /// - `words`: byval datums verbatim; byref offsets shifted by the heap
    ///   base for VALID rows (null rows stay 0, as serial pushes them);
    /// - `validity`: whole-word concatenation — legal only because chunk
    ///   seams fall on 64-row boundaries (the caller's contract; enforced);
    /// - constancy: part stays constant iff both sides are constant and
    ///   their first canonical values agree (a value-free side is neutral);
    /// - `hash`: [`LogicalColHash::merge`] — wrapping-add lanes, so replay
    ///   in input order is bit-identical to serial observes;
    /// - counters (`rows`/`nonnull`/`value_bytes`/`oversize_values`/
    ///   `has_null`): sums/or.
    ///
    /// Contract: same schema; `self.rows % 64 == 0` (a partial chunk is only
    /// ever the LAST splice — the final chunk of the final part).
    pub fn splice_chunk(&mut self, chunk: &ColBuffer) -> WriteResult<()> {
        if chunk.schema != self.schema {
            return Err(WriteError::Contract {
                detail: "splice of a foreign-schema chunk",
            });
        }
        if self.rows % 64 != 0 {
            return Err(WriteError::Contract {
                detail: "splice seam off the 64-row validity-word boundary",
            });
        }
        // D2 side-channel merge (before counters move): both sides must
        // cover their rows or the part's channel drops whole. Chunk source
        // ordinals translate into the accumulator's source table (dedup by
        // Arc identity — chunks off one source row group share one Arc).
        // Deterministic: chunks splice in INPUT order, so the part source
        // table is first-use ordered — a pure function of the input.
        self.dict_side = match (self.rows, self.dict_side.take(), &chunk.dict_side) {
            (0, _, Some(cs)) if cs.rows.len() as u64 == chunk.rows => Some(DictSide {
                sources: cs.sources.clone(),
                rows: cs.rows.clone(),
            }),
            (_, Some(mut acc), Some(cs))
                if acc.rows.len() as u64 == self.rows
                    && cs.rows.len() as u64 == chunk.rows =>
            {
                let mut xlat: memledger::ChargedVec<u32> = memledger::ChargedVec::new(self.owner(), memledger::Kind::Region);
                xlat.try_reserve(cs.sources.len()).map_err(|_| WriteError::Format(pgrc_format::FormatError::OutOfMemory { bytes: cs.sources.len() * core::mem::size_of::<u32>() }))?;
                for s in &cs.sources {
                    let idx = match acc
                        .sources
                        .iter()
                        .position(|a| std::sync::Arc::ptr_eq(a, s))
                    {
                        Some(i) => i,
                        None => {
                            acc.sources.push(s.clone());
                            acc.sources.len() - 1
                        }
                    };
                    xlat.with_vec(|v| v.push(idx as u32)); // alloc: Exec charged self.owner()
                }
                pgrc_format::try_reserve_more(&mut acc.rows, cs.rows.len(), "ingest")?; // alloc: Exec charged self.charge
                for &slot in &cs.rows {
                    acc.rows.push(if slot == DICT_ROW_PLAIN { // alloc: Exec charged self.charge
                        DICT_ROW_PLAIN
                    } else {
                        let src = (slot >> 32) as u32;
                        let code = slot as u32;
                        (u64::from(xlat[src as usize]) << 32) | u64::from(code)
                    });
                }
                Some(acc)
            }
            _ => None,
        };
        let byref = matches!(
            self.schema.class,
            StorageClass::Fixed { .. } | StorageClass::VarlenaVerbatim
        );
        if byref {
            // Serial pads before each ENTRY, never after the last one: a
            // value-free chunk contributes no pad, so only pad when the
            // chunk actually carries heap bytes.
            if !chunk.heap.is_empty() {
                pad8(&mut self.heap);
            }
            let base = self.heap.len() as u64;
            self.heap.extend_from_slice(&chunk.heap); // alloc: Exec charged self.charge
            pgrc_format::try_reserve_more(&mut self.words, chunk.words.len(), "ingest")?; // alloc: Exec charged self.charge
            for r in 0..chunk.rows as usize {
                if chunk.is_valid(r) {
                    self.words.push(chunk.words[r] + base); // alloc: Exec charged self.charge
                } else {
                    self.words.push(chunk.words[r]); // alloc: Exec charged self.charge
                }
            }
        } else {
            self.words.extend_from_slice(&chunk.words); // alloc: Exec charged self.charge
        }
        self.validity.extend_from_slice(&chunk.validity); // alloc: Exec charged self.charge
        self.utf8_proven = chunk.utf8_proven && (self.rows == 0 || self.utf8_proven);
        self.has_null |= chunk.has_null;
        self.rows += chunk.rows;
        self.nonnull += chunk.nonnull;
        self.value_bytes += chunk.value_bytes;
        self.oversize_values += chunk.oversize_values;
        self.oversize_bytes += chunk.oversize_bytes;
        match (&self.first_canon, &chunk.first_canon) {
            (_, None) => {
                // Value-free chunk (all null or empty): constancy-neutral.
            }
            (None, Some(f)) => {
                self.constant = self.constant && chunk.constant;
                self.first_canon = Some(f.clone());
            }
            (Some(a), Some(b)) => {
                self.constant = self.constant && chunk.constant && a == b;
            }
        }
        self.hash.merge(&chunk.hash);
        self.sync_charge();
        Ok(())
    }

    /// Build the `EncodeInput` for granule `g` over materialized pointers,
    /// at the part's elected `grain`.
    pub fn encode_input<'a>(
        &'a self,
        g: u32,
        grain: GranuleGrain,
        ptrs: &'a [u64],
        rows_g: u32,
    ) -> EncodeInput<'a> {
        EncodeInput {
            class: self.schema.class,
            rows: rows_g,
            datums: ptrs,
            validity: self.granule_validity(g, grain),
        }
    }
}

/// Rows per equality granule of [`ColBuffer::append_word_lane`]'s digest fold.
const WORD_GRANULE: usize = 16;

/// [`normalize_varlena`]'s `Plain4B` arm alone: `None` = any other form.
#[inline]
fn plain_4b_payload(image: &[u8]) -> Option<&[u8]> {
    let head: [u8; 4] = image.get(..4)?.try_into().ok()?;
    if head[0] & VARATT_MASK_4B != VARATT_4B_U {
        return None;
    }
    let total = (u32::from_le_bytes(head) >> 2) as usize;
    image.get(4..total)
}

/// Set bits `lo..hi` of an LSB-first bitset.
fn set_bits(v: &mut [u64], lo: usize, hi: usize) {
    let mut r = lo;
    while r < hi {
        let w = r >> 6;
        let b = r & 63;
        let n = (64 - b).min(hi - r);
        let m = if n == 64 { u64::MAX } else { ((1u64 << n) - 1) << b };
        v[w] |= m;
        r += n;
    }
}

fn pad8(buf: &mut Vec<u8>) {
    let rem = buf.len() % 8;
    if rem != 0 {
        buf.resize(buf.len() + (8 - rem), 0); // alloc: Exec charged the caller ColBuffer.charge (heap pad)
    }
}

#[cfg(test)]
mod lane_tests {
    use super::*;
    use crate::testkit::{bool_col, fixed16_col, img_4b_u, int8_col, text_col};

    fn word_col(class: StorageClass) -> ColSchema {
        ColSchema { class, ..int8_col(1) }
    }

    fn same(a: &ColBuffer, b: &ColBuffer) {
        assert_eq!(a.words, b.words);
        assert_eq!(a.heap, b.heap);
        assert_eq!(a.validity, b.validity);
        assert_eq!(
            (a.has_null, a.rows, a.nonnull, a.value_bytes, a.oversize_values, a.oversize_bytes, a.constant),
            (b.has_null, b.rows, b.nonnull, b.value_bytes, b.oversize_values, b.oversize_bytes, b.constant)
        );
        assert_eq!(a.first_canon, b.first_canon);
        assert_eq!(a.hash.digest(), b.hash.digest());
        assert_eq!(a.logical_hash().unwrap().digest(), b.logical_hash().unwrap().digest());
    }

    fn lcg(state: &mut u64) -> u64 {
        *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *state >> 33
    }

    fn slices(n: usize, seed: u64) -> Vec<(usize, usize)> {
        let mut st = seed;
        let (mut out, mut lo) = (Vec::new(), 0);
        while lo < n {
            let hi = (lo + 1 + (lcg(&mut st) % 90) as usize).min(n);
            out.push((lo, hi));
            lo = hi;
        }
        out
    }

    #[test]
    fn word_lane_equals_per_row_appends() {
        let classes = [
            int8_col(1).class,
            StorageClass::ByvalWord { width: 4, signed: true },
            StorageClass::ByvalWord { width: 2, signed: false },
            StorageClass::ByvalWord { width: 1, signed: true },
            StorageClass::F32,
            StorageClass::F64,
            bool_col(1).class,
        ];
        for (ci, class) in classes.into_iter().enumerate() {
            for mode in 0..7u64 {
                let mut st = 77 + ci as u64 * 13 + mode;
                let n = 1000 + (lcg(&mut st) % 300) as usize;
                let mut words = Vec::new();
                let mut nulls = Vec::new();
                for i in 0..n {
                    let w = match mode {
                        0 => 7,
                        1 => lcg(&mut st) << 20 | lcg(&mut st),
                        2 | 4 => (i / 37) as u64 | 0xffff_0000_0000_0000,
                        // equal within the width, different above it
                        5 => (i / 23) as u64 | (lcg(&mut st) << 40),
                        // granule-aligned runs and runs of one row mixed
                        6 => if (i / 16) % 3 == 0 { lcg(&mut st) } else { (i / 16) as u64 },
                        _ => lcg(&mut st) % 3,
                    };
                    words.push(w);
                    nulls.push(matches!(mode, 1..=3) && lcg(&mut st) % 9 == 0 || (mode == 3 && i < 70));
                }
                let mut a = ColBuffer::new(word_col(class));
                for i in 0..n {
                    if nulls[i] {
                        a.append_null();
                    } else {
                        a.append_word(words[i]).unwrap();
                    }
                }
                let mut b = ColBuffer::new(word_col(class));
                b.reserve_rows(n, 0).unwrap();
                for (lo, hi) in slices(n, st) {
                    b.append_word_lane(&words[lo..hi], &nulls[lo..hi]).unwrap();
                }
                same(&a, &b);
            }
        }
    }

    #[test]
    fn image_lane_equals_per_row_appends() {
        for (schema, fixed) in [(text_col(1), false), (fixed16_col(1), true)] {
            let mut st = 5u64;
            let n = 700;
            let (mut arena, mut spans, mut nulls) = (Vec::new(), Vec::new(), Vec::new());
            for i in 0..n {
                let null = lcg(&mut st) % 7 == 0;
                let payload: Vec<u8> = if fixed {
                    (0..16).map(|k| (i * 3 + k) as u8).collect()
                } else if i == 300 {
                    vec![b'x'; OVERSIZE_THRESHOLD as usize + 5]
                } else {
                    (0..(lcg(&mut st) % 40) as usize).map(|k| b'a' + (k % 26) as u8).collect()
                };
                let img = if fixed { payload } else { img_4b_u(&payload) };
                arena.extend_from_slice(b"zz");
                spans.push((arena.len() as u32, img.len() as u32));
                arena.extend_from_slice(&img);
                nulls.push(null);
            }
            let mut ext = NoExternalDetoast;
            let mut scratch = Vec::new();
            let mut a = ColBuffer::new(schema);
            for i in 0..n {
                if nulls[i] {
                    a.append_null();
                    continue;
                }
                let (off, len) = spans[i];
                let image = &arena[off as usize..(off + len) as usize];
                if fixed {
                    a.append_fixed(image).unwrap();
                } else {
                    let (p, _) = normalize_varlena(image, &mut ext, &mut scratch).unwrap();
                    a.append_varlena_payload(p).unwrap();
                }
            }
            let mut b = ColBuffer::new(schema);
            for (lo, hi) in slices(n, 9) {
                b.append_image_lane(&arena, &spans[lo..hi], &nulls[lo..hi], &mut ext, &mut scratch).unwrap();
            }
            same(&a, &b);
        }
    }
}
