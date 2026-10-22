//! Global dictionary build (spec §7; chunk M3-D row: "global dict build
//! (byte-rank-sorted, framed, stored lengths)").
//!
//! One framed, **byte-rank-sorted** global dictionary per dict column per
//! part. Global codes ARE the stored codes — there is no stitch pass and no
//! remap layer to build here, by design (charter §6). The builder observes
//! every non-null value of the part, sorts distinct images by raw byte rank,
//! assigns codes 0..n in that order (sorted order is contractual: code
//! compare = value compare where an order embedding exists), and emits the
//! two dict stream sections:
//!
//! - `DictIndex`: [`DictIndexEntry`] × entry_count (12 B fixed stride,
//!   closed-form addressing — no frame table);
//! - `DictPayload`: varlena-shaped, 8-aligned entries in byte-rank order,
//!   framed at [`DICT_FRAME_ENTRIES`] entries per dict frame (the section's
//!   frame table marks each dict frame's start for the reader's lazy
//!   `ensure_frame` handle).
//!
//! Both sections carry `EncodingId::DictCodes` in their headers — the dict
//! streams belong to the DICT_CODES encoding family; layout is role-driven
//! (spec §6.1 roles 4/5). NOTE for A/C/F lanes: spec §7 does not pin the
//! `encoding` byte of dict stream sections; this writer emits DICT_CODES and
//! the choice is recorded in the lane report as a freeze-clarification
//! candidate.
//!
//! At M3-D no value stream elects DICT_CODES (the §6.11 code-stream encoder
//! is M3-C's, `lanev3-m3-chunks.md` §2 M3-C row), so the builder is not yet
//! wired into the default election set; it is built and gate-tested here so
//! C's election can consume it unchanged ([`crate::elect`] seam).
//!
//! DICT-DEDUP (the #971 follow-up finding — the DISTACC-KILL recipe applied
//! HERE): the original distinct-image structure was the `BTreeMap` below —
//! every observation paid an O(log D) tree descent of byte compares, the
//! same memcmp lane DISTACC-KILL measured at ~13% of ALL ingest CPU on the
//! meta side, still alive on exactly the columns where it is most expensive
//! (dict-elected strings). The builder now maintains the distinct set in an
//! open-addressing hash table keyed on a private 64-bit folded-multiply
//! hash of the payload (`dict_fast_hash`, PGRC-WRITER-FAST `dicth`: the
//! table's hash never reaches part bytes, so it need not be the meta
//! plane's `meta_hash128`): an observation is one masked probe, a hash
//! compare, and ONE byte-equality confirm on the hash match (byte equality
//! still adjudicates identity — a collision cannot merge values). The byte-rank order the BTreeMap provided for free is recovered
//! by ONE build-time sort over the surviving distinct set (D log D once per
//! part, not N log D during observe); both arms converge on the same
//! byte-ordered build, so hash iteration order never reaches part bytes.
//!
//! The builder also counts each distinct value's exact non-null occurrences
//! (one add on the entry already in hand). That counted set is the dedup's
//! second face: on dict-elected columns the seal driver feeds it to the
//! meta builder (`pgrc_meta::sketch::distribution_from_sorted_counts`), so
//! the part's distinct set is maintained ONCE — the dict build — with two
//! consumers (dict sections + the Stats sidecar distribution), instead of
//! twice over the same bytes.
//!
//! Kill switch: `PGRUST_PGRC_DICT_HASH=0|off` restores the BTreeMap
//! reference arm (the A/B control and the equivalence witness; both arms
//! are pinned output-equal — entries, ranks, handles, counts — by the
//! adversarial battery in `tests/dict_tests.rs`).
//!
//! Determinism: the BTree arm's byte order IS its iteration order; the hash
//! arm's build sorts the distinct set into the identical byte-rank order (a
//! total order — entries are distinct) — no hash-order leak into part bytes
//! (the byte-identical-parts law, charter §1).

use pgrc_format::dict::{dict_entry, DictCharLenForm, DictIndexEntry, DictSections, DICT_INDEX_ENTRY_LEN};
use pgrc_format::enc::{EncodingId, Wrapper};
use pgrc_format::geom::DICT_FRAME_ENTRIES;
use pgrc_format::part::{StreamCloseout, StreamSectionWriter, STREAM_SECTION_HDR_LEN};
use pgrc_format::wire::put_varlena_entry;
use std::collections::BTreeMap;

use crate::held::{cap_bytes, HeldCharge};
use crate::{WriteError, WriteResult};

/// The DICT-DEDUP arm switch (read at builder construction — the
/// `sketch.rs` DISTACC parse verbatim: unset or anything but "0"/"off" =
/// hash arm).
fn hash_arm_on() -> bool {
    !matches!(
        std::env::var("PGRUST_PGRC_DICT_HASH").as_deref(),
        Ok("0") | Ok("off")
    )
}

/// How `char_len` is derived from an entry's payload bytes (spec §7: entries
/// carry stored byte- AND char-lengths so `length()` is a table lookup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextSemantics {
    /// UTF-8 character count (text/varchar classes).
    Utf8Chars,
    /// `char_len == byte_len` (bytea and other byte-semantics classes).
    BytesOnly,
}

fn char_len_of(payload: &[u8], sem: TextSemantics) -> u32 {
    match sem {
        TextSemantics::BytesOnly => payload.len() as u32,
        TextSemantics::Utf8Chars => {
            // Count UTF-8 lead bytes; ill-formed input degrades to a byte
            // count for the malformed tail (typed-refusal-free: char stats
            // are metadata, never a value surface).
            payload.iter().filter(|&&b| (b & 0xC0) != 0x80).count() as u32
        }
    }
}

/// Per-entry facts staged in the builder map (SEAL-FUSION: `insert_id` is
/// the observe-time handle — first-observation order — that turns code
/// assignment into a rank read instead of a second payload walk + binary
/// search; the rank mapping is minted at [`DictBuilder::build`].
/// DICT-DEDUP: `count` is the value's exact non-null occurrence count —
/// the Stats-sidecar distribution's input, maintained here because the
/// entry is already in hand at every observation).
#[derive(Debug, Clone, Copy)]
struct EntryFacts {
    char_len: u32,
    insert_id: u32,
    count: u64,
}

/// One distinct value in the hash arm: the precomputed table hash, the
/// payload bytes (stored once, on first sight — the confirm + the
/// build-sort key), and the same facts the BTree arm stages. Vec index ==
/// `insert_id` (push order IS first-observation order).
#[derive(Debug)]
struct HashEntry {
    h1: u64,
    payload: Vec<u8>,
    char_len: u32,
    count: u64,
}

#[derive(Debug)]
enum DictAcc {
    /// Default: open addressing over `dict_fast_hash(payload)`. `slots`
    /// holds index+1 into `entries` (0 = empty); power-of-two capacity,
    /// linear probing, grown below 3/4 load (the `sketch.rs` DISTACC-KILL
    /// table's idiom verbatim). Probe compares the stored hash first;
    /// bytes are compared exactly once, on the hash match that
    /// adjudicates identity.
    Hash {
        slots: Vec<u32>,
        entries: Vec<HashEntry>,
    },
    /// `PGRUST_PGRC_DICT_HASH=0|off`: the original byte-keyed tree (kept
    /// verbatim as the A/B control / equivalence witness).
    BTree(BTreeMap<Vec<u8>, EntryFacts>),
}

/// CONVERT W3-2b: new distinct payload bytes a builder takes before it syncs its charge between table growths (the
/// bounded lag of [`DictBuilder::observe`]'s charge: at most this much past the last sync).
const DICT_SYNC_BYTES: u64 = 64 << 10;

/// Accumulates the distinct value set of one dict column over a part.
#[derive(Debug)]
pub struct DictBuilder {
    /// CONVERT W3-2b: the part's owner's charge on [`DictBuilder::held_bytes`] ([`DictBuilder::charged`]; none by
    /// default). Declared first: it drops before the tables are freed.
    charge: HeldCharge,
    /// CONVERT W3-2b: the distinct payloads' bytes (each stored once, `to_vec`: capacity = length).
    pay: u64,
    /// CONVERT W3-2b: `pay` and the tables' capacities (slots, entries, row handles) at the charge's last sync.
    synced: (u64, usize, usize, usize),
    /// payload bytes → facts (both arms build the same byte-rank order).
    acc: DictAcc,
    sem: TextSemantics,
    observed_rows: u64,
    /// SEAL-FUSION: one handle per OBSERVED (non-null) row, observe order.
    row_handles: Vec<u32>,
    /// K1-MEM: the distinct entries' payload bytes (the dictionary's payload floor, [`DictBuilder::distinct_bytes`]).
    distinct_bytes: u64,
    /// PGRC-WRITER-FAST `run`: the hash-arm entry the previous observe
    /// landed on, + 1 (0 = none).
    last: u32,
}

impl Default for DictBuilder {
    fn default() -> DictBuilder {
        DictBuilder::new(TextSemantics::default())
    }
}

impl Default for TextSemantics {
    fn default() -> TextSemantics {
        TextSemantics::BytesOnly
    }
}

/// The built, code-assigned dictionary.
#[derive(Debug)]
pub struct BuiltDict {
    /// CONVERT W3-2b: the builder's charge, synced to [`BuiltDict::held_bytes`] (none on the D2 inherit path).
    /// Declared first: it drops before the entries are freed.
    charge: HeldCharge,
    /// CONVERT W3-2b: the entries' payload bytes.
    pay: u64,
    /// Byte-rank-sorted payloads; index == global code.
    entries: Vec<(Vec<u8>, u32)>,
    /// DICT-DEDUP: per entry (code order), its exact non-null occurrence
    /// count — the Stats-sidecar distribution feed. Empty on the D2
    /// inherit path ([`BuiltDict::from_sorted_entries`] never observes
    /// rows, so it has no counts to offer; that path's distribution stays
    /// with the meta accumulator).
    counts: Vec<u64>,
    /// SEAL-FUSION: insert_id → byte-rank code (the observe-handle map).
    rank_of: Vec<u32>,
    /// SEAL-FUSION: per observed (non-null) row, its entry's insert_id.
    row_handles: Vec<u32>,
}

/// The emitted dict section pair + closeout facts for extent records.
#[derive(Debug)]
pub struct DictSectionImages {
    pub index_section: Vec<u8>,
    pub index_closeout: StreamCloseout,
    pub payload_section: Vec<u8>,
    pub payload_closeout: StreamCloseout,
    pub entry_count: u32,
    /// The [`DictCharLenForm`] the index's `char_field` was emitted in
    /// (M5d char-len record); the seal stamps the matching stream flag.
    pub charlen_form: DictCharLenForm,
}

impl DictBuilder {
    pub fn new(sem: TextSemantics) -> DictBuilder {
        DictBuilder::with_hash_arm(sem, hash_arm_on())
    }

    /// Arm-explicit constructor (the equivalence battery + the env default
    /// above).
    pub fn with_hash_arm(sem: TextSemantics, hash: bool) -> DictBuilder {
        DictBuilder {
            charge: HeldCharge::none(),
            pay: 0,
            synced: (0, 0, 0, 0),
            acc: if hash {
                DictAcc::Hash {
                    slots: Vec::new(),
                    entries: Vec::new(),
                }
            } else {
                DictAcc::BTree(BTreeMap::new())
            },
            sem,
            observed_rows: 0,
            row_handles: Vec::new(),
            distinct_bytes: 0,
            last: 0,
        }
    }

    /// CONVERT W3-2b: the builder charges its tables and payloads to `owner` (the part's: the seal passes its column's
    /// [`crate::ingest::ColBuffer::owner`]), kind `Region`; `None`: uncharged.
    pub fn charged(mut self, owner: Option<memledger::Owner>) -> DictBuilder {
        self.charge = HeldCharge::new(owner);
        self.sync_charge();
        self
    }

    /// CONVERT W3-2b: the heap bytes the builder holds (its tables' capacity and the distinct payloads; the tree arm
    /// counts its entries' payloads and facts, not the tree's nodes).
    pub fn held_bytes(&self) -> u64 {
        let tables = match &self.acc {
            DictAcc::Hash { slots, entries } => {
                slots.capacity() * 4 + entries.capacity() * core::mem::size_of::<HashEntry>()
            }
            DictAcc::BTree(map) => {
                map.len() * map.iter().next().map_or(0, |(k, v)| core::mem::size_of_val(k) + core::mem::size_of_val(v))
            }
        };
        (tables + self.row_handles.capacity() * 4) as u64 + self.pay
    }

    fn caps(&self) -> (usize, usize, usize) {
        match &self.acc {
            DictAcc::Hash { slots, entries } => (slots.capacity(), entries.capacity(), self.row_handles.capacity()),
            DictAcc::BTree(map) => (0, map.len(), self.row_handles.capacity()),
        }
    }

    fn sync_charge(&mut self) {
        let (a, b, c) = self.caps();
        self.synced = (self.pay, a, b, c);
        self.charge.sync(self.held_bytes());
    }

    /// Observe one non-null value's payload bytes (varlena header EXCLUDED —
    /// the canonical value bytes of spec §18.1). Records the row→entry
    /// handle (SEAL-FUSION) so code assignment after [`DictBuilder::build`]
    /// is a rank read, never a second payload walk, and the entry's exact
    /// occurrence count (DICT-DEDUP — one add on the entry in hand).
    pub fn observe(&mut self, payload: &[u8]) {
        self.observed_rows += 1;
        let id = match &mut self.acc {
            DictAcc::BTree(map) => match map.get_mut(payload) {
                Some(f) => {
                    f.count += 1;
                    f.insert_id
                }
                None => {
                    let id = map.len() as u32;
                    let cl = char_len_of(payload, self.sem);
                    self.distinct_bytes += payload.len() as u64;
                    self.pay += payload.len() as u64;
                    map.insert( // alloc: Exec charged self.charge
                        payload.to_vec(), // alloc: Exec charged self.charge
                        EntryFacts {
                            char_len: cl,
                            insert_id: id,
                            count: 1,
                        },
                    );
                    id
                }
            },
            DictAcc::Hash { slots, entries } => 'hash: {
                // PGRC-WRITER-FAST `run`: a payload byte-equal to the
                // previous row's entry counts there (no hash, no probe).
                if self.last != 0 {
                    let e = &mut entries[(self.last - 1) as usize];
                    if e.payload.as_slice() == payload {
                        e.count += 1;
                        break 'hash self.last - 1;
                    }
                }
                // Grow below 3/4 load (also the lazy init: 0 slots grows
                // to the initial table before the first probe).
                if (entries.len() + 1) * 4 > slots.len() * 3 {
                    grow(slots, entries); // alloc: Exec charged self.charge (synced below: the slots' capacity moved)
                }
                let h1 = dict_fast_hash(payload);
                let mask = slots.len() - 1;
                let mut i = pgrc_meta::hash::slot_index(h1, mask);
                loop {
                    let s = slots[i];
                    if s == 0 {
                        debug_assert!(entries.len() < u32::MAX as usize);
                        let id = entries.len() as u32;
                        self.distinct_bytes += payload.len() as u64;
                        self.pay += payload.len() as u64;
                        entries.push(HashEntry { // alloc: Exec charged self.charge
                            h1,
                            payload: payload.to_vec(), // alloc: Exec charged self.charge
                            char_len: char_len_of(payload, self.sem),
                            count: 1,
                        });
                        slots[i] = id + 1;
                        break id;
                    }
                    let e = &mut entries[(s - 1) as usize];
                    if e.h1 == h1 && e.payload.as_slice() == payload {
                        e.count += 1;
                        break s - 1;
                    }
                    i = (i + 1) & mask;
                }
            }
        };
        if matches!(self.acc, DictAcc::Hash { .. }) {
            self.last = id + 1;
        }
        self.row_handles.push(id); // alloc: Exec charged self.charge
        // CONVERT W3-2b: sync when a table moved or past DICT_SYNC_BYTES of new payloads (a repeat costs compares)
        if self.charge.owner().is_some()
            && (self.pay - self.synced.0 >= DICT_SYNC_BYTES || {
                let (a, b, c) = self.caps();
                (a, b, c) != (self.synced.1, self.synced.2, self.synced.3)
            })
        {
            self.sync_charge();
        }
    }

    pub fn distinct(&self) -> usize {
        match &self.acc {
            DictAcc::BTree(map) => map.len(),
            DictAcc::Hash { entries, .. } => entries.len(),
        }
    }

    pub fn observed_rows(&self) -> u64 {
        self.observed_rows
    }

    /// K1-MEM: the payload bytes of the distinct values observed. The dictionary's sections can never be smaller:
    /// DictPayload stores every entry's bytes (in a varlena entry), DictIndex 12 B per entry.
    pub fn distinct_bytes(&self) -> u64 {
        self.distinct_bytes
    }

    /// Freeze the code assignment: BTreeMap iteration IS byte-rank order;
    /// the hash arm recovers the identical order with ONE sort over the
    /// distinct set (total order — entries are distinct, so both arms
    /// mint the same entries/ranks/counts; the battery pins it). The
    /// insert_id → rank map is minted here (SEAL-FUSION), so
    /// `code_of_observed(k)` equals `code_of(payload_of_row_k)` by
    /// construction — both ARE the byte rank.
    pub fn build(self) -> BuiltDict {
        let n = self.distinct();
        let mut charge = self.charge;
        let pay = self.pay;
        let handles = self.row_handles.capacity() as u64 * 4;
        // D4594: charge first: each table the build allocates is on the charge before its allocation (a block counted
        // after it sat unowned through the work between)
        let out_bytes = (n * core::mem::size_of::<(Vec<u8>, u32)>()) as u64;
        charge.sync(charge.charged() + (n * 4 + n * 8) as u64);
        let mut rank_of = vec![0u32; n]; // alloc: Exec charged charge
        let mut counts = vec![0u64; n]; // alloc: Exec charged charge
        let entries: Vec<(Vec<u8>, u32)> = match self.acc {
            DictAcc::BTree(map) => {
                charge.sync(charge.charged() + out_bytes);
                map.into_iter()
                    .enumerate()
                    .map(|(rank, (bytes, f))| {
                        rank_of[f.insert_id as usize] = rank as u32;
                        counts[rank] = f.count;
                        (bytes, f.char_len)
                    })
                    .collect() // alloc: Exec charged charge
            }
            DictAcc::Hash { entries, slots } => {
                // CONVERT W3-2b: the probe table is done with (it went at the builder's drop before)
                charge.sync(charge.charged().saturating_sub(slots.capacity() as u64 * 4));
                drop(slots);
                charge.sync(charge.charged() + (entries.len() * core::mem::size_of::<(Vec<u8>, u32, u64, u32)>()) as u64);
                // (payload, char_len, count, insert_id), byte-rank sorted.
                let mut staged: Vec<(Vec<u8>, u32, u64, u32)> = entries
                    .into_iter()
                    .enumerate()
                    .map(|(id, e)| (e.payload, e.char_len, e.count, id as u32))
                    .collect(); // alloc: Exec charged charge
                // CONVERT W3-2b: the build's peak: the staged table beside the rank / count maps and the handles
                charge.sync(
                    pay + handles
                        + cap_bytes(staged.capacity(), &staged)
                        + (rank_of.capacity() * 4 + counts.capacity() * 8) as u64,
                );
                // PGRC-WRITER-FAST `sort`: an 8-byte prefix key first, the
                // same total byte order.
                // CONVERT W3-2a (cvformat A3): the sort's keys / runs / permutation on the build's owner
                pgrc_meta::sketch::sort_bytes_prefixed_for(
                    &mut staged,
                    |t| &t.0,
                    charge.owner().map(|o| (o, memledger::Kind::Region)),
                );
                charge.sync(charge.charged() + out_bytes);
                staged
                    .into_iter()
                    .enumerate()
                    .map(|(rank, (bytes, cl, count, id))| {
                        rank_of[id as usize] = rank as u32;
                        counts[rank] = count;
                        (bytes, cl)
                    })
                    .collect() // alloc: Exec charged charge
            }
        };
        let mut built = BuiltDict {
            charge,
            pay,
            entries,
            counts,
            rank_of,
            row_handles: self.row_handles,
        };
        built.sync_charge();
        built
    }
}

/// Grow (or lazily create) the slot table and rehash by the STORED h1 —
/// no byte access on the rehash path (`sketch.rs`'s grow verbatim).
fn grow(slots: &mut Vec<u32>, entries: &[HashEntry]) {
    let ncap = if slots.is_empty() { 16 } else { slots.len() * 2 };
    let mask = ncap - 1;
    let mut ns = vec![0u32; ncap];
    for (idx, e) in entries.iter().enumerate() {
        let mut i = pgrc_meta::hash::slot_index(e.h1, mask);
        while ns[i] != 0 {
            i = (i + 1) & mask;
        }
        ns[i] = idx as u32 + 1;
    }
    *slots = ns;
}

impl BuiltDict {
    /// SEAL-SPEED-2 D2: construct directly from an already byte-rank-sorted,
    /// strictly-distinct entry list (the inherited-dictionary merge's
    /// output). The strict-ascending check is the STRICT-DISTINCTNESS CERT
    /// re-proven mechanically per part — a violation is a merge bug and
    /// refuses typed (never normalizes; charter §3). `char_len`s are
    /// computed here at ENTRY grain (the rebuild path pays this per part
    /// row; D2 pays it once per distinct value).
    ///
    /// The canonical-form law this leans on (and re-proves through the
    /// seal's twin gates + the rig dirshas): the byte-rank dict is a pure
    /// function of the part's distinct value SET, so a dict built from
    /// inherited source structure equals the rebuilt dict byte-for-byte
    /// whenever the entry sets are equal — which the referenced-entry
    /// filter guarantees (every kept entry is some row's value; every
    /// row's value is kept).
    ///
    /// No observe-time row handles exist on this path
    /// ([`BuiltDict::code_of_observed`] is the REBUILD path's face); D2
    /// code streams arrive by remap instead.
    pub fn from_sorted_entries(
        entries: Vec<Vec<u8>>,
        sem: TextSemantics,
    ) -> WriteResult<BuiltDict> {
        if !entries.windows(2).all(|w| w[0] < w[1]) {
            return Err(WriteError::Contract {
                detail: "inherited dict merge broke strict byte-rank order",
            });
        }
        let entries: Vec<(Vec<u8>, u32)> = entries
            .into_iter()
            .map(|bytes| {
                let cl = char_len_of(&bytes, sem);
                (bytes, cl)
            })
            .collect();
        Ok(BuiltDict {
            charge: HeldCharge::none(),
            pay: 0,
            entries,
            counts: Vec::new(),
            rank_of: Vec::new(),
            row_handles: Vec::new(),
        })
    }

    /// CONVERT W3-2a: an inherited-merge dictionary ([`BuiltDict::from_sorted_entries`]) charged to `owner` (the part's
    /// owner) from here until it drops.
    pub fn charged(mut self, owner: Option<memledger::Owner>) -> BuiltDict {
        self.charge = HeldCharge::new(owner);
        self.pay = self.entries.iter().map(|(b, _)| b.capacity() as u64).sum();
        self.sync_charge();
        self
    }

    /// CONVERT W3-2b: the heap bytes the dictionary holds (its tables' capacity and its entries' payloads).
    pub fn held_bytes(&self) -> u64 {
        let tables = cap_bytes(self.entries.capacity(), &self.entries) as usize
            + self.counts.capacity() * 8
            + self.rank_of.capacity() * 4
            + self.row_handles.capacity() * 4;
        tables as u64 + self.pay
    }

    fn sync_charge(&mut self) {
        self.charge.sync(self.held_bytes());
    }

    /// CONVERT W3-2b: the owner the dictionary is charged to (`None`: uncharged).
    pub fn owner(&self) -> Option<memledger::Owner> {
        self.charge.owner()
    }

    pub fn entry_count(&self) -> u32 {
        self.entries.len() as u32
    }

    /// DICT-DEDUP: consume the dictionary into its counted distinct set —
    /// (payload bytes, exact non-null occurrence count) in byte-rank order
    /// — the Stats-sidecar distribution feed
    /// (`pgrc_meta::sketch::distribution_from_sorted_counts`'s contract:
    /// strictly byte-ascending, Σ counts = the part's non-null rows).
    /// `None` on the D2 inherit path, which never observed rows and has no
    /// counts (its distribution stays with the meta accumulator — same
    /// sidecar bytes, both are pure functions of the data). Call LAST: the
    /// sections must already be emitted and verified.
    pub fn into_counted_entries(self) -> Option<Vec<(Vec<u8>, u64)>> {
        if self.counts.len() != self.entries.len() {
            return None;
        }
        Some(
            self.entries
                .into_iter()
                .zip(self.counts)
                .map(|((bytes, _char_len), count)| (bytes, count))
                .collect(),
        )
    }

    /// Global code of a payload (binary search over the sorted entries).
    pub fn code_of(&self, payload: &[u8]) -> Option<u32> {
        self.entries
            .binary_search_by(|(e, _)| e.as_slice().cmp(payload))
            .ok()
            .map(|i| i as u32)
    }

    /// SEAL-FUSION: global code of the k-th OBSERVED (non-null) row — a
    /// rank read through the observe-time handle. Equals
    /// `code_of(payload_of_that_row)` by construction (module doc on
    /// [`DictBuilder::build`]); `None` = the driver asked past the
    /// observed rows (a code bug, refused typed by the caller).
    pub fn code_of_observed(&self, k: usize) -> Option<u32> {
        self.row_handles
            .get(k)
            .map(|&id| self.rank_of[id as usize])
    }

    /// The byte-rank-sorted invariant (contractual, spec §7): strictly
    /// ascending byte order — also proves no duplicates.
    pub fn is_byte_rank_sorted(&self) -> bool {
        self.entries.windows(2).all(|w| w[0].0 < w[1].0)
    }

    /// Emit the DictIndex + DictPayload stream sections (spec §7 framing).
    ///
    /// `form` picks the index `char_field` form (the M5d char-len record):
    /// Absolute = the blessed lineage byte-for-byte (the dirsha posture);
    /// Delta = `byte_len − char_len` (measured decline; never underflows —
    /// `char_len_of` counts lead bytes, ≤ byte_len by construction);
    /// Absent = zeros (Option-C: nothing stored, consumers recompute).
    /// Recorded on the returned images; the seal stamps the stream flag.
    pub fn emit_sections(&self, form: DictCharLenForm) -> WriteResult<DictSectionImages> {
        self.emit_sections_on(form, &mut HeldCharge::none(), 0)
    }

    /// [`BuiltDict::emit_sections`] with the two images and the offset table on `charge`, which already holds `held`
    /// bytes of its caller's. Each buffer is sized from the entries and charged before its fill (and charged again if a
    /// fill grows it past that size), so no section byte sits on the account without an owner (D4575). The caller keeps
    /// the charge with the images; [`crate::elect::DictPlan`] counts them in its held bytes.
    pub fn emit_sections_on(
        &self,
        form: DictCharLenForm,
        charge: &mut HeldCharge,
        held: u64,
    ) -> WriteResult<DictSectionImages> {
        let enc = EncodingId::DictCodes.as_u16();
        let n = self.entries.len();
        let oom = |bytes: usize| WriteError::Format(pgrc_format::FormatError::OutOfMemory { bytes });

        // DictPayload: varlena-shaped 8-aligned entries in code order,
        // begin_frame every DICT_FRAME_ENTRIES entries. Collect each entry's
        // payload-region-relative header offset for the index.
        // Its exact bound: the header, each entry padded to 8, the frame table.
        let payload_len = STREAM_SECTION_HDR_LEN
            + self.entries.iter().map(|(b, _)| (4 + b.len()).next_multiple_of(8)).sum::<usize>()
            + n.div_ceil(DICT_FRAME_ENTRIES as usize) * 4;
        let mut payload_section: Vec<u8> = Vec::new(); // alloc: Exec charged charge
        payload_section.try_reserve_exact(payload_len).map_err(|_| oom(payload_len))?; // alloc: Exec charged charge
        let mut offs: Vec<u32> = Vec::new(); // alloc: Exec charged charge
        offs.try_reserve_exact(n).map_err(|_| oom(n * 4))?; // alloc: Exec charged charge
        let mut fitted = payload_section.capacity() + offs.capacity() * 4;
        charge.sync(held + fitted as u64);
        let payload_closeout = {
            let mut w =
                StreamSectionWriter::begin(&mut payload_section, enc, 0, Wrapper::None)
                    .map_err(WriteError::Format)?;
            for (code, (bytes, _)) in self.entries.iter().enumerate() {
                if code as u32 % DICT_FRAME_ENTRIES == 0 {
                    w.align_payload(8);
                    w.begin_frame();
                }
                // Entries are 8-aligned relative to the payload region;
                // sections start 8-aligned in the file (writer law), so the
                // alignment is absolute (spec §1).
                w.align_payload(8);
                let off = w.payload_off();
                let base = w.payload().len() - off as usize;
                let abs = put_varlena_entry(w.payload(), bytes); // alloc: Exec charged charge
                offs.push((abs as usize - base) as u32); // alloc: Exec charged charge
                let now = w.payload().capacity() + offs.capacity() * 4;
                if now != fitted {
                    fitted = now;
                    charge.sync(held + fitted as u64);
                }
            }
            // One "granule" close per dict frame keeps the closeout value
            // count == entry count without abusing row geometry: dict
            // streams are not row streams, so gcounts are not emitted
            // (child = false), but the closeout still reports values.
            w.end_granule(self.entries.len() as u32);
            w.finish(false).map_err(WriteError::Format)?
        };

        // DictIndex: 12-B fixed-stride records; closed-form addressing.
        let index_len = STREAM_SECTION_HDR_LEN + n * DICT_INDEX_ENTRY_LEN;
        let mut index_section: Vec<u8> = Vec::new(); // alloc: Exec charged charge
        index_section.try_reserve_exact(index_len).map_err(|_| oom(index_len))?; // alloc: Exec charged charge
        let mut fitted = payload_section.capacity() + offs.capacity() * 4 + index_section.capacity();
        charge.sync(held + fitted as u64);
        let index_closeout = {
            let mut w = StreamSectionWriter::begin(&mut index_section, enc, 0, Wrapper::None)
                .map_err(WriteError::Format)?;
            for (code, (bytes, char_len)) in self.entries.iter().enumerate() {
                let byte_len = bytes.len() as u32;
                let e = DictIndexEntry {
                    payload_off: offs[code],
                    byte_len,
                    char_field: match form {
                        DictCharLenForm::Absolute => *char_len,
                        DictCharLenForm::Delta => byte_len - char_len,
                        DictCharLenForm::Absent => 0,
                    },
                };
                e.encode_into(w.payload());
                let now = payload_section.capacity() + offs.capacity() * 4 + w.payload().capacity();
                if now != fitted {
                    fitted = now;
                    charge.sync(held + fitted as u64);
                }
            }
            w.end_granule(self.entries.len() as u32);
            w.finish(false).map_err(WriteError::Format)?
        };
        // The offset table's bytes leave the charge before its block is freed.
        charge.sync(held + (payload_section.capacity() + index_section.capacity()) as u64);
        drop(offs);

        Ok(DictSectionImages {
            index_section,
            index_closeout,
            payload_section,
            payload_closeout,
            entry_count: self.entry_count(),
            charlen_form: form,
        })
    }

    /// Round-trip verification vs the oracle (the M3-D slice leg): resolve
    /// every code through the FORMAT's own `dict_entry` reader over the
    /// emitted sections and compare payload bytes + lengths against the
    /// builder's entries. Returns the number of codes verified.
    pub fn verify_sections(&self, images: &DictSectionImages) -> WriteResult<u32> {
        let d = DictSections {
            index: section_payload(&images.index_section)?,
            payload: section_payload(&images.payload_section)?,
            entry_count: images.entry_count,
            charlen_form: images.charlen_form,
            form: pgrc_format::dict::DictPayloadForm::Expanded,
        };
        for (code, (bytes, char_len)) in self.entries.iter().enumerate() {
            let e = dict_entry(&d, code as u32).map_err(WriteError::Format)?;
            if e.bytes != bytes.as_slice() || e.byte_len as usize != bytes.len() {
                return Err(WriteError::Contract {
                    detail: "dict round-trip payload mismatch",
                });
            }
            if e.char_len != *char_len {
                return Err(WriteError::Contract {
                    detail: "dict round-trip char_len mismatch",
                });
            }
        }
        Ok(self.entry_count())
    }
}

/// The payload region of an emitted dict section (past the 32-B header,
/// before the frame table when present). Crate-visible: the seal driver
/// builds `DictSections` verify ctxs over the emitted images (M3-A2).
pub(crate) fn section_payload(section: &[u8]) -> WriteResult<&[u8]> {
    let hdr =
        pgrc_format::part::StreamSectionHdr::decode(section).map_err(WriteError::Format)?;
    let end = if hdr.frame_table_off != 0 {
        hdr.frame_table_off as usize
    } else {
        section.len()
    };
    section
        .get(pgrc_format::part::STREAM_SECTION_HDR_LEN..end)
        .ok_or(WriteError::Contract {
            detail: "dict section payload bounds",
        })
}

/// PGRC-WRITER-FAST `dicth`: the dict builder's private table hash — a
/// wyhash-style folded-multiply (one 64×64→128 fold per 16 bytes). Build
/// sorts the entries by bytes and byte equality adjudicates identity, so
/// the table hash never reaches part bytes.
#[inline(always)]
fn fold_mul(a: u64, b: u64) -> u64 {
    let r = u128::from(a) * u128::from(b);
    (r as u64) ^ ((r >> 64) as u64)
}

#[inline]
fn dict_fast_hash(b: &[u8]) -> u64 {
    const P0: u64 = 0xa076_1d64_78bd_642f;
    const P1: u64 = 0xe703_7ed1_a0b4_28db;
    const P2: u64 = 0x8ebc_6af0_9c88_c6e3;
    let mut seed = P0 ^ (b.len() as u64).wrapping_mul(P2);
    let mut chunks = b.chunks_exact(16);
    for c in &mut chunks {
        let w0 = u64::from_le_bytes(c[..8].try_into().expect("8"));
        let w1 = u64::from_le_bytes(c[8..].try_into().expect("8"));
        seed = fold_mul(w0 ^ P1, w1 ^ seed);
    }
    let r = chunks.remainder();
    let mut t = [0u8; 16];
    t[..r.len()].copy_from_slice(r);
    let w0 = u64::from_le_bytes(t[..8].try_into().expect("8"));
    let w1 = u64::from_le_bytes(t[8..].try_into().expect("8"));
    seed = fold_mul(w0 ^ P1, w1 ^ seed);
    fold_mul(seed ^ P2, (b.len() as u64) ^ P1)
}
