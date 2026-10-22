//! A program's sites and output columns: `SiteDesc`, `SiteKind`, `OutCol`, table and shred declarations (split out of `program/mod.rs` by type, contract-runtime landing 3).

use super::*;

pub const NAME_LEN: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SiteKind {
    /// `worker` (D3014O, nest.parallel): a worker may probe the site (an embedded nest inner's plain btree probe whose
    /// outer may widen, ops/nestloop/widen.rs): a worker binds the site's frozen-image probe (driver bind, hosts
    /// index_probe.rs); the leader's own source serves it on the leader.
    /// `leaves`: an order-free btree Index Only Scan the pool reads (decompose closure.rs `index_only_leaves`): every
    /// participant, the leader too, seizes leaf pages of one shared parallel btree scan (hosts index_leaves.rs).
    /// `ranges`: a btree scan whose index order the statement's emit keeps, read by the pool in key ranges (decompose
    /// closure.rs `index_ranges`): each claim is a run of ranges cut at the index's own separator keys, read in index
    /// order, and the emit hands the claims on in claim order (hosts index_ranges.rs). Its last `RANGE_KEYS` key cells
    /// hold the claim, past the key words.
    IndexSource { nkeys: u16, natts: u16, worker: bool, leaves: bool, ranges: bool },
    /// `nkeys`: the site's key cells (`RANGE_KEYS` unless a TABLESAMPLE's computed argument reads more executor parameters).
    /// `zkeys`: the last `zkeys` of them hold open-time zone comparands (PS1-05, road `nest.eagersafe`: an eager-safe
    /// InitPlan's value the source's open stores, src.verdicts' key-cell slots `word_zone_key_arg`), so the host counts its
    /// own cells (the tid range's, the sample's) without them.
    RangeSource { natts: u16, nkeys: u16, zkeys: u16 },
    Write { natts: u16 },
    /// A program's frame words (lowerv3 design §3.6, D229): `n` key cells kept across calls and arms, zeroed per execution.
    Words { n: u16 },
    /// D732: a COPY row mark's row (a VALUES / function scan's whole row): no host source; its position (the row's datum,
    /// stored by the write or lock) is kept like a source's and handed to an EvalPlanQual recheck's node.
    Copy,
    /// D776: the row a write's or lock's REFERENCE mark names in source site `of`'s own scan (its position is the window's
    /// base): stored here per row, and the settle keeps it as site `of`'s position for an EvalPlanQual recheck.
    RowOf { of: u16 },
    None,
}

pub const RANGE_KEYS: usize = 2;

/// A declared table lane's per-entry function (milestone-2.md §2.6 item 38, the hosts' `TableFn`): table `i` is lane `natts + 1 + i`, built by the source per part over code lane `over`; `LIKE`'s pattern and the
/// derived fingerprint's `regexp_replace` pattern / replacement are `args` (`table_bytes` slices).
pub mod table_fn {
    pub const FINGERPRINT: u8 = 0;
    pub const OCTET_LENGTH: u8 = 1;
    pub const CHAR_LENGTH: u8 = 2;
    pub const LIKE: u8 = 3;
    pub const DERIVED_FINGERPRINT: u8 = 4;
    pub const PROGRAM: u8 = 5; // program-filled at bind (a pure body of one dictionary column): `flags` the element, `args[0]` the body's 8-byte fingerprint
    /// B2-20: a code set, the `Bits` table of the entries whose bytes equal one of `args[0]`'s values (`put_values`),
    /// `FLAG_NEGATED` of the others.
    pub const EQ: u8 = 6;
    /// P5-06 / SRC-17: a byte interval, the `Bits` table of the entries whose bytes are at or after `args[0]`'s first value
    /// and, when it has a second, before that one (`put_values`): one code run of a byte-ordered dictionary.
    pub const RANGE: u8 = 7;
    /// An immutable builtin of the entry and baked varlena constants, fingerprinted per entry by the source: `args[0]` the
    /// call's image (`Call::encode`).
    pub const DERIVED_CALL: u8 = 8;
    /// OID-19, with `FLAG_BPCHAR` only: an `I64` table of each entry's rank among the part's entries ordered by their bytes
    /// without trailing blanks (`bcTruelen`), entries equal after the trim sharing one rank.
    pub const RANK: u8 = 9;
    pub const FLAG_ILIKE: u8 = 1;
    /// On `DERIVED_FINGERPRINT`: `regexp_replace`'s flag `'g'`, every match replaced (PostgreSQL's 4-argument form with
    /// `'g'`), not only the first.
    pub const FLAG_GLOBAL: u8 = 1;
    pub const FLAG_NEGATED: u8 = 2;
    /// On `FINGERPRINT` (D767, V4R-266): the entries' renders without trailing blanks (`bcTruelen`), bpchar's equality image.
    pub const FLAG_BPCHAR: u8 = 4;
    /// On `FINGERPRINT` (textkeys step 3, lowerv4 road `key.canon`): each entry's canonical text key image
    /// (`gravity_dict::fp::canon_of`: its bytes up to 15, else its Fp128 under bit 63) in place of its Fp128, the trimmed
    /// render's with `FLAG_BPCHAR`; the same 16 bytes a code.
    pub const FLAG_CANON: u8 = 8;
    /// On `EQ` (X5): more values are bound per execution, each read from the site's key cell that `args[1]` names
    /// (`put_cells`): the program writes a parameter's word there at the window's bind, before the table's lane fill; the
    /// source reads each value as a text image, and a NULL or unreadable one makes every entry a fault (the row path).
    /// On `LIKE` (PS1-15): the pattern is `args[0]`'s constant runs (`put_values`, one more than the cells) with each
    /// cell's value between two runs, assembled by the source the same way.
    pub const FLAG_BOUND: u8 = 8;
    /// On `DERIVED_CALL` ((2'), DK1-MISEST): the lowering predicted that a filter keeps few of the part's entries, so the
    /// source chooses per part: the hosted build (every entry, as without the flag) once the source's previous part's rows
    /// filled at least half of its entries, else the table unfilled as a `PROGRAM` Fp128 table that the rows fill (the
    /// program's lazy sweep: every entry `[0, 0]`, each row's re-evaluation stores its entry).
    pub const FLAG_ADAPTIVE: u8 = 16;
    /// On `RANGE` with `FLAG_BOUND` (NEW-72 entry C): the bound text constant in key cell `args[1]`'s one cell is the
    /// interval's upper end (`[.., c)`), else its lower end (`[c, ..)`).
    pub const FLAG_BELOW: u8 = 32;
    /// On `RANGE` with `FLAG_BOUND`: the end is `c || '\0'` (`x <= c`, `x > c`: no byte string lies between the two).
    pub const FLAG_NUL: u8 = 64;
    /// On `EQ` with `FLAG_BOUND` (P5-02): each key cell holds a whole text array (an `IN` list past `MAX_LIST`, one word
    /// whatever its length): the source reads every element as a value; a NULL element or an unreadable image makes every
    /// entry a fault (the row path).
    pub const FLAG_LIST: u8 = 128;
    /// On `PROGRAM` (D3115; the element is the flags' low bits): the program sweeps the table at the bind (not the lazy
    /// rows' fill), so the statement's sources that reach a part share one table, each sweeping the chunks it claims.
    pub const FLAG_SWEPT: u8 = 128;
    /// On `OCTET_LENGTH` (D3038 A): every read the site's pipeline makes of the code lane's column is a length call or the
    /// emptiness test (lowerv4 octet_lane.rs `sole`). Where every table over the lane is such an `OCTET_LENGTH` table, a
    /// verbatim part that sealed the column's octet-length lane codes the lane by length from that lane, never its text
    /// (hosts text_hosts `lane_codes`, exec sites.rs `lens_only`).
    pub const FLAG_SOLE: u8 = 1;

    /// `FLAG_BOUND`'s argument: each key cell as a 2-byte little-endian index.
    pub fn put_cells(cells: &[u16], out: &mut impl FnMut(&[u8])) {
        for c in cells {
            out(&c.to_le_bytes());
        }
    }

    /// The cells `put_cells` wrote; `None` for an argument it did not write.
    pub fn cells(arg: &[u8]) -> Option<Vec<u16>> {
        (arg.len() % 2 == 0).then(|| arg.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()) // alloc: Program
    }

    /// `EQ`'s and `RANGE`'s argument: each value as its 4-byte little-endian length, then its bytes, handed to `out` in pieces
    /// (the caller's buffer, in its own context).
    pub fn put_values<V: AsRef<[u8]>>(values: &[V], out: &mut impl FnMut(&[u8])) {
        for v in values {
            let v = v.as_ref();
            out(&(v.len() as u32).to_le_bytes());
            out(v);
        }
    }

    /// The values `put_values` wrote; `None` for an argument it did not write.
    pub fn values(mut arg: &[u8]) -> Option<Vec<&[u8]>> {
        let mut out = Vec::new(); // alloc: Program
        while !arg.is_empty() {
            let n = u32::from_le_bytes(arg.get(..4)?.try_into().ok()?) as usize;
            out.push(arg.get(4..4 + n)?); // alloc: Program
            arg = &arg[4 + n..];
        }
        Some(out)
    }

    /// `put_values`' argument `arg` holds at least one value, each of byte length 0: an equality with them is a function
    /// of the byte length alone (exec sites.rs `lens_only`).
    pub fn values_all_empty(arg: &[u8]) -> bool {
        !arg.is_empty() && arg.len() % 4 == 0 && arg.iter().all(|&b| b == 0)
    }

    /// `DERIVED_CALL`'s call, evaluated once per entry: builtin `funcid` over `args`, where `None` is the entry and
    /// `Some` a constant's varlena payload (the lowering bakes it: a by-value literal stays a program word, D407).
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    pub struct Call {
        pub funcid: u32,
        pub args: Vec<Option<Vec<u8>>>, // alloc: Program
    }

    impl Call {
        const ENTRY: u32 = u32::MAX;

        /// The table-argument image: `funcid`, the argument count, then per argument its length (`ENTRY` for the entry) and bytes.
        pub fn encode(&self) -> Vec<u8> {
            let mut out = Vec::with_capacity(5 + self.args.iter().map(|a| 4 + a.as_ref().map_or(0, Vec::len)).sum::<usize>()); // alloc: Program
            out.extend_from_slice(&self.funcid.to_le_bytes());
            out.push(self.args.len() as u8);
            for a in &self.args {
                out.extend_from_slice(&a.as_ref().map_or(Self::ENTRY, |b| b.len() as u32).to_le_bytes());
                out.extend_from_slice(a.as_deref().unwrap_or(&[]));
            }
            out
        }

        /// `encode`'s inverse; `None` for an image it did not write or a call with no entry argument.
        pub fn decode(b: &[u8]) -> Option<Call> {
            let funcid = u32::from_le_bytes(b.get(..4)?.try_into().ok()?);
            let n = *b.get(4)? as usize;
            let (mut at, mut args) = (5usize, Vec::with_capacity(n)); // alloc: Program
            for _ in 0..n {
                let len = u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?);
                at += 4;
                args.push(if len == Self::ENTRY { None } else { Some(b.get(at..at + len as usize)?.to_vec()) }); // alloc: Program
                at += if len == Self::ENTRY { 0 } else { len as usize };
            }
            (at == b.len() && args.iter().any(Option::is_none)).then_some(Call { funcid, args })
        }
    }

    #[cfg(test)]
    mod call_tests {
        use super::Call;

        /// A call's image decodes to the same call; an image with no entry argument, a short one or one with bytes past
        /// its last argument is no call.
        #[test]
        fn a_call_image_round_trips() {
            let c = Call { funcid: 2087, args: vec![None, Some(b"http://".to_vec()), Some(Vec::new())] };
            assert_eq!(Call::decode(&c.encode()), Some(c.clone()));
            let lone = Call { funcid: 870, args: vec![None] };
            assert_eq!(Call::decode(&lone.encode()), Some(lone));
            assert_eq!(Call::decode(&Call { funcid: 1, args: vec![Some(b"x".to_vec())] }.encode()), None, "no entry argument");
            let img = c.encode();
            assert_eq!(Call::decode(&img[..img.len() - 1]), None, "short");
            assert_eq!(Call::decode(&[img.as_slice(), &[0]].concat()), None, "trailing bytes");
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TableDecl {
    pub over: u8,
    pub func: u8,
    pub flags: u8,
    pub collation: u32,
    pub encoding: i32,
    pub args: [(u32, u32); 2],
}

impl TableDecl {
    pub const fn fingerprint(over: u8) -> TableDecl {
        TableDecl { over, func: table_fn::FINGERPRINT, flags: 0, collation: 0, encoding: 0, args: [(0, 0); 2] }
    }

    /// A bpchar code lane's `Fp128` table: `fingerprint`'s over the trimmed renders (hashbpchar's image; D767, V4R-266).
    pub const fn fingerprint_bpchar(over: u8) -> TableDecl {
        TableDecl { flags: table_fn::FLAG_BPCHAR, ..TableDecl::fingerprint(over) }
    }

    /// textkeys step 3: a code lane's canonical image table (`table_fn::FLAG_CANON`), the trimmed renders' for a bpchar lane.
    pub const fn fingerprint_canon(over: u8, bpchar: bool) -> TableDecl {
        let base = if bpchar { TableDecl::fingerprint_bpchar(over) } else { TableDecl::fingerprint(over) };
        TableDecl { flags: base.flags | table_fn::FLAG_CANON, ..base }
    }

    /// Whether this is a code lane's key image table: `fingerprint`, `fingerprint_bpchar` or a `fingerprint_canon` (the
    /// lane's `DictRef::fp_table`).
    pub fn is_lane_fingerprint(&self) -> bool {
        let canon = self.flags & table_fn::FLAG_CANON != 0;
        [false, true].into_iter().any(|bp| *self == if canon { TableDecl::fingerprint_canon(self.over, bp) } else if bp { TableDecl::fingerprint_bpchar(self.over) } else { TableDecl::fingerprint(self.over) })
    }

    /// A bpchar code lane's trimmed rank table (OID-19): the order `bpcharcmp` gives a byte-ordered collation.
    pub const fn rank_bpchar(over: u8) -> TableDecl {
        TableDecl { func: table_fn::RANK, ..TableDecl::fingerprint_bpchar(over) }
    }
}

/// M6 D2: the kinds of a shredded jsonb path lane (pgrc's `ShredLaneKind`, `ShredDecl::kind`).
pub mod shred_kind {
    pub const TEXT: u8 = 0;
    pub const UUID16: u8 = 1;
    pub const NUMERIC_FS: u8 = 2;
    pub const BOOL: u8 = 3;
    /// A Text path read as its dictionary codes (R13-L03, jsonb.codes R3): the lane a code lane (`set_codes_lane`).
    pub const TEXT_CODES: u8 = 4;
    /// jsonops (3): a Text path's presence, a byval word per row (`?` / `?|` / `?&` of the path's key): 1 where the path's
    /// lane row is valid, 0 where it is invalid in a part sealed STREAMF_SHRED_OBJECT_ROOTS (every document an object, so
    /// the key is absent); NULL where the document is NULL and in every part without the flag or the path (unknown).
    pub const PRESENCE: u8 = 5;
    /// D3038: a varlena column's payload byte length per row (`octet_length` of text / bytea), a byval word: the host
    /// reads it from the column's own streams (a verbatim part's entry headers, a coded part's dictionary index), valid
    /// where the column is not NULL. Its path is empty; it is never a jsonb path's.
    pub const OCTET_LENGTH: u8 = 6;
    /// D3038 (R2): a varlena column's emptiness per row (`col <> ''`), a byval word: 1 where its payload holds a byte, 0
    /// where it is empty, NULL where the column is; read from the column's own streams as `OCTET_LENGTH` is. Its path is
    /// empty; it is never a jsonb path's.
    pub const NONEMPTY: u8 = 7;
    /// D3038 (R3): a text column's character length per row (`length` / `char_length`), a byval word read as
    /// `OCTET_LENGTH` is where the column's granule is all ASCII (its Stats witness), counted from the decoded value
    /// elsewhere; NULL where the column is. Its path is empty; it is never a jsonb path's.
    pub const CHAR_LENGTH: u8 = 8;
}

/// M6 D2 (L0-g): attribute lane `lane` (its id `LaneId::JsonbPath(n)` for the site's `n`-th declaration, its out attno the
/// jsonb column's) is the shredded path of the root object's key `path` (a `table_bytes` slice) at `kind`
/// (`shred_kind`) and number `scale`. The host publishes it from each part that elected the path at that kind and scale,
/// and all-invalid from any other part: an invalid row reads the column's image. `claims`: the compares of the site's first
/// filter that read the path through the lane (step/jsonb_inplace.rs); when they are every read of the column there, the
/// column's lane is late and the host fills its image only where a path lane is invalid (source/pgrc_late.rs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ShredDecl {
    pub lane: u8,
    pub kind: u8,
    pub scale: i16,
    pub path: (u32, u32),
    pub claims: u16,
}

/// A site's shape, sized per program (milestone-2.md §2.6 item 30): the lists live in the arena the builders were given and `Program::with_pipelines` copies them into the program's own.
#[derive(PartialEq, Eq)]
pub struct SiteDesc {
    pub kind: SiteKind,
    pub relation: u32,
    pub index: u32,
    pub key_attnos: ArenaVec<i16>,
    pub param_slots: ArenaVec<i16>,
    pub out_attnos: ArenaVec<i16>,
    pub lanes: ArenaVec<LaneDesc>,
    pub driving_lanes: LaneSet,
    /// The zone predicates the source may answer per window, one per distinct predicate, as many as the quals give (sized per program).
    pub zone: ArenaVec<ZonePred>,
    /// D3009: the disjunctions the source may skip a window by, each an OR qual's arms as one predicate per arm (an arm
    /// implies its predicate): a window is `NoneMatch` when every arm's predicate is, never `AllMatch` (the qual's filter
    /// still runs on every window read). `zone_any_ends[i]` is the end of group `i` in `zone_any`. Kept apart from `zone`,
    /// which every road reads as a conjunction: only the host's granule verdict reads these.
    pub zone_any: ArenaVec<ZonePred>,
    pub zone_any_ends: ArenaVec<u16>,
    /// Lanes the kernel reads only through the registered zone predicates: a source may leave them undecoded (`ptr` null) in an `AllMatch` window.
    pub filter_lanes: LaneSet,
    /// Lanes whose aggregate the kernel takes from the window's `Answer` record when the source answers the window (addition f): with every lane filter-only or answered, an `AllMatch` window needs no decode.
    pub answer_lanes: LaneSet,
    /// The code lanes' dictionaries serve point probes only (a bounded run's n renders, a part's empty code): the host opens them sparse — block faults, never a whole stream per part (lower.md §15.12).
    pub sparse_dicts: bool,
    /// P5-18 / SRC-23: the renders a sparse site's code lane can get at most: a bounded emit's rows (a LIMIT or a top-k,
    /// offset included). The host weighs each part's share of them against streaming the part's dictionary whole
    /// (`pgrc_read::wrapped_dense_pays`, on the part's own block count and device regime) before it opens, and opens the
    /// other parts at block grain with the bytes-touched election live. `u64::MAX`: no bound (every part opens for probes);
    /// 0: a site that declares its probes (the entries window, the heads), whose dictionaries never stream.
    pub dict_renders: u64,
    /// The code lanes' dictionary heads are prefaulted at a part's first granule (the hand CB25 recipe's `with_head_prefault`): the heads' n renders per part touch the head blocks, never a whole stream.
    pub head_prefault: bool,
    /// A heap range published as raw pages (the page pointer, the visible offsets, the page flags): the program deforms; the lanes are filled on demand (`lane_fill`) for a page the flags do not clear.
    pub raw_pages: bool,
    /// PGRCSERVE G4: a range site over the table AM's slot scan (the rows road, hosts am_rows.rs) in the heap range
    /// protocol, serial: `table_scan_road` answered `Rows`, or a heap twin under `pgrust.gravity_rows_road`.
    pub rows_via_am: bool,
    /// The entries window (lower.md §15.18): this code lane's dictionary-coded, delete-free parts are published as their dictionary ENTRIES (one row per code in rank order, `FLAG_ENTRIES`), not their rows; any other part walks its
    /// rows. The declarer's result is the same over either (a set of the lane's values: `count(DISTINCT text)`).
    /// `Some(ENTRIES_EVERY)`: every lane is a code lane published so, side by side in one window (row `i` holds each
    /// lane's `i`-th code of the window, invalid past that lane's own entries): a declarer that reads each lane's value set
    /// alone (`count(DISTINCT a), count(DISTINCT b)`, PS1-19), never a row's pairing of two lanes.
    pub entries: Option<u8>,
    /// PS1-18: the entries window's pipeline filters its entries: the source never takes a part whole as entry runs, which
    /// would skip the filter (hosts `entry_runs_offer`).
    pub entries_filtered: bool,
    /// RD-dict.entries: the entries window's declarer keeps a NULL value as a value of its set (`SELECT DISTINCT x`,
    /// `GROUP BY x` with no aggregate), so a part whose lane holds a NULL publishes one invalid row after its entries
    /// (hosts `stage_entries`, from the part's stats records; a part without them walks its rows), and is never taken whole
    /// as entry runs.
    pub entries_nulls: bool,
    /// D2981: the entries window publishes each part's codes in descending rank order (window 0 holds the part's
    /// highest codes, each window's codes descend): the declarer is a top whose first key descends over the lane
    /// (code.heads), so code.cut's early end takes its windows as an ascending top takes the rank order. Ignored by
    /// `ENTRIES_EVERY`.
    pub entries_desc: bool,
    pub tables: ArenaVec<TableDecl>,
    pub table_bytes: ArenaVec<u8>,
    /// M6 D2: the attribute lanes that are shredded jsonb paths (`ShredDecl`).
    pub shreds: ArenaVec<ShredDecl>,
    /// D3475: a heap range read in place of the plan's full btree walk (decompose closure.rs `full_range`: an order-free
    /// Index Scan or Index Only Scan with no index qual): the walk's index and whether the walk is index-only. The source
    /// counts the walk's statistics as PostgreSQL 18.6 does (`idx_scan` / `idx_tup_read` / `idx_tup_fetch` on the index, no
    /// sequential scan), so the statistics views keep the plan's counts. In the content key, a sequential scan of the same
    /// graph never shares this site's recipe. `None`: a sequential scan's counts. D3581: on the bitmap morsel form of an
    /// order-free Index Scan (closure.rs `index_morsel`), the index its heap fetches count on (`idx_tup_fetch`).
    pub walk: Option<(u32, bool)>,
    /// D3585: a heap range over a relation set (`pgrust.gravity_relset`): the twin partition scans of one Append read as
    /// one site, `set[0]` the site's `relation`, every relation of the same physical layout and read by the same clauses;
    /// the claims tile the relations' blocks in this order, a claim never crossing a relation (exec runner plan.rs
    /// `claims_of`, hosts `RelSet`). Empty: the site's one relation.
    pub set: ArenaVec<u32>,
    /// D3663: the set's Append prunes at the executor start (initial steps only): the index of its `partPruneInfos` entry,
    /// `set[i]` its subplan `i`. Each execution reads the members that entry keeps (exec append/proto.rs `Initial`).
    pub set_prune: Option<u32>,
    /// sortmerge (D4469 road 2): the merge key of a pgrc range site the plan relies on for an order (decompose, from the
    /// plan's `SeqScan.pgrc_merge`), its attnos in key order, ASC NULLS LAST. The host merges the statement snapshot's
    /// parts' sorted streams (hosts `MergedRangeSource`), so the site's rows come in that order at any part count; the
    /// site is serial, its key lanes values, and it takes no answers, entries or walk order. Empty: an unordered site.
    pub merge: MergeAttnos,
}

/// sortmerge: a merged site's key, attnos in key order, held inline (at most `limits::KEYS`: a key is a `sorted_by`
/// prefix). It reads, hashes, compares and prints as the slice of its attnos.
#[derive(Clone, Copy)]
pub struct MergeAttnos {
    n: u8,
    keys: [i16; crate::limits::KEYS],
}

impl MergeAttnos {
    /// No key: an unordered site.
    pub const NONE: MergeAttnos = MergeAttnos { n: 0, keys: [0; crate::limits::KEYS] };

    /// The key `keys`; `None` past `limits::KEYS` attnos.
    pub fn of(keys: &[i16]) -> Option<MergeAttnos> {
        let mut m = MergeAttnos::NONE;
        m.keys.get_mut(..keys.len())?.copy_from_slice(keys);
        m.n = keys.len() as u8;
        Some(m)
    }
}

impl core::ops::Deref for MergeAttnos {
    type Target = [i16];
    fn deref(&self) -> &[i16] {
        &self.keys[..self.n as usize]
    }
}

impl PartialEq for MergeAttnos {
    fn eq(&self, other: &MergeAttnos) -> bool {
        **self == **other
    }
}

impl Eq for MergeAttnos {}

impl core::hash::Hash for MergeAttnos {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

impl core::fmt::Debug for MergeAttnos {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        (**self).fmt(f)
    }
}

/// The derived form's clone, by hand: each list clones (in its own arena), every other field is a copy.
impl Clone for SiteDesc {
    fn clone(&self) -> SiteDesc {
        let SiteDesc { kind, relation, index, key_attnos, param_slots, out_attnos, lanes, driving_lanes, zone, zone_any, zone_any_ends, filter_lanes, answer_lanes, sparse_dicts, dict_renders, head_prefault, raw_pages, rows_via_am, entries, entries_filtered, entries_nulls, entries_desc, tables, table_bytes, shreds, walk, set, set_prune, merge } = self;
        SiteDesc {
            kind: *kind,
            relation: *relation,
            index: *index,
            key_attnos: key_attnos.clone(),
            param_slots: param_slots.clone(),
            out_attnos: out_attnos.clone(),
            lanes: lanes.clone(),
            driving_lanes: driving_lanes.clone(),
            zone: zone.clone(),
            zone_any: zone_any.clone(),
            zone_any_ends: zone_any_ends.clone(),
            filter_lanes: filter_lanes.clone(),
            answer_lanes: answer_lanes.clone(),
            sparse_dicts: *sparse_dicts,
            dict_renders: *dict_renders,
            head_prefault: *head_prefault,
            raw_pages: *raw_pages,
            rows_via_am: *rows_via_am,
            entries: *entries,
            entries_filtered: *entries_filtered,
            entries_nulls: *entries_nulls,
            entries_desc: *entries_desc,
            tables: tables.clone(),
            table_bytes: table_bytes.clone(),
            shreds: shreds.clone(),
            walk: *walk,
            set: set.clone(),
            set_prune: *set_prune,
            merge: *merge,
        }
    }
}

/// The derived form's output and hash over every field but `set`, `set_prune` and `merge`, which join them only when they
/// hold a relation (D3585), an index (D3663) or a key (sortmerge): a one-relation unordered site's dumps and content keys
/// are those of a descriptor without them. Both name every field (a
/// field added to the descriptor is a compile error here until it is printed and hashed; `rows_via_am` only when set).
impl core::fmt::Debug for SiteDesc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let SiteDesc { kind, relation, index, key_attnos, param_slots, out_attnos, lanes, driving_lanes, zone, zone_any, zone_any_ends, filter_lanes, answer_lanes, sparse_dicts, dict_renders, head_prefault, raw_pages, rows_via_am, entries, entries_filtered, entries_nulls, entries_desc, tables, table_bytes, shreds, walk, set, set_prune, merge } = self;
        let mut d = f.debug_struct("SiteDesc");
        d
            .field("kind", kind)
            .field("relation", relation)
            .field("index", index)
            .field("key_attnos", key_attnos)
            .field("param_slots", param_slots)
            .field("out_attnos", out_attnos)
            .field("lanes", lanes)
            .field("driving_lanes", driving_lanes)
            .field("zone", zone)
            .field("zone_any", zone_any)
            .field("zone_any_ends", zone_any_ends)
            .field("filter_lanes", filter_lanes)
            .field("answer_lanes", answer_lanes)
            .field("sparse_dicts", sparse_dicts)
            .field("dict_renders", dict_renders)
            .field("head_prefault", head_prefault)
            .field("raw_pages", raw_pages)
            .field("entries", entries)
            .field("entries_filtered", entries_filtered)
            .field("entries_nulls", entries_nulls)
            .field("entries_desc", entries_desc)
            .field("tables", tables)
            .field("table_bytes", table_bytes)
            .field("shreds", shreds)
            .field("walk", walk);
        if *rows_via_am {
            d.field("rows_via_am", rows_via_am);
        }
        if !set.is_empty() {
            d.field("set", set);
        }
        if let Some(p) = set_prune {
            d.field("set_prune", p);
        }
        if !merge.is_empty() {
            d.field("merge", merge);
        }
        d.finish()
    }
}

impl core::hash::Hash for SiteDesc {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        let SiteDesc { kind, relation, index, key_attnos, param_slots, out_attnos, lanes, driving_lanes, zone, zone_any, zone_any_ends, filter_lanes, answer_lanes, sparse_dicts, dict_renders, head_prefault, raw_pages, rows_via_am, entries, entries_filtered, entries_nulls, entries_desc, tables, table_bytes, shreds, walk, set, set_prune, merge } = self;
        kind.hash(state);
        relation.hash(state);
        index.hash(state);
        key_attnos.hash(state);
        param_slots.hash(state);
        out_attnos.hash(state);
        lanes.hash(state);
        driving_lanes.hash(state);
        zone.hash(state);
        zone_any.hash(state);
        zone_any_ends.hash(state);
        filter_lanes.hash(state);
        answer_lanes.hash(state);
        sparse_dicts.hash(state);
        dict_renders.hash(state);
        head_prefault.hash(state);
        raw_pages.hash(state);
        entries.hash(state);
        entries_filtered.hash(state);
        entries_nulls.hash(state);
        entries_desc.hash(state);
        tables.hash(state);
        table_bytes.hash(state);
        shreds.hash(state);
        walk.hash(state);
        // PGRCSERVE G4: a rows-road site joins the key only when it is one (other sites keep trunk's keys)
        if *rows_via_am {
            rows_via_am.hash(state);
        }
        if !set.is_empty() {
            set.hash(state);
        }
        if let Some(p) = set_prune {
            p.hash(state);
        }
        if !merge.is_empty() {
            merge.hash(state);
        }
    }
}

pub(crate) fn copy_vec<T: Clone>(v: &[T], a: ArenaRef) -> ArenaVec<T> {
    let mut o = ArenaVec::with_capacity_in(v.len(), a);
    o.extend_from_slice(v);
    o
}

pub(crate) const fn round8(n: usize) -> usize {
    n.div_ceil(8) * 8
}

impl SiteDesc {
    /// `entries`' value for every lane at once (PS1-19).
    pub const ENTRIES_EVERY: u8 = u8::MAX;

    pub const fn none_in(a: ArenaRef) -> SiteDesc {
        SiteDesc { kind: SiteKind::None, relation: 0, index: 0, key_attnos: ArenaVec::new_in(a), param_slots: ArenaVec::new_in(a), out_attnos: ArenaVec::new_in(a), lanes: ArenaVec::new_in(a), driving_lanes: LaneSet::new_in(a), zone: ArenaVec::new_in(a), zone_any: ArenaVec::new_in(a), zone_any_ends: ArenaVec::new_in(a), filter_lanes: LaneSet::new_in(a), answer_lanes: LaneSet::new_in(a), sparse_dicts: false, dict_renders: 0, head_prefault: false, raw_pages: false, rows_via_am: false, entries: None, entries_filtered: false, entries_nulls: false, entries_desc: false, tables: ArenaVec::new_in(a), table_bytes: ArenaVec::new_in(a), shreds: ArenaVec::new_in(a), walk: None, set: ArenaVec::new_in(a), set_prune: None, merge: MergeAttnos::NONE }
    }

    /// The descriptor with its lists in arena `a` (the program's, at build).
    pub fn copy_in(&self, a: ArenaRef) -> SiteDesc {
        SiteDesc { kind: self.kind, relation: self.relation, index: self.index, key_attnos: copy_vec(&self.key_attnos, a), param_slots: copy_vec(&self.param_slots, a), out_attnos: copy_vec(&self.out_attnos, a), lanes: copy_vec(&self.lanes, a), driving_lanes: self.driving_lanes.copy_in(a), zone: copy_vec(&self.zone, a), zone_any: copy_vec(&self.zone_any, a), zone_any_ends: copy_vec(&self.zone_any_ends, a), filter_lanes: self.filter_lanes.copy_in(a), answer_lanes: self.answer_lanes.copy_in(a), sparse_dicts: self.sparse_dicts, dict_renders: self.dict_renders, head_prefault: self.head_prefault, raw_pages: self.raw_pages, rows_via_am: self.rows_via_am, entries: self.entries, entries_filtered: self.entries_filtered, entries_nulls: self.entries_nulls, entries_desc: self.entries_desc, tables: copy_vec(&self.tables, a), table_bytes: copy_vec(&self.table_bytes, a), shreds: copy_vec(&self.shreds, a), walk: self.walk, set: copy_vec(&self.set, a), set_prune: self.set_prune, merge: self.merge }
    }

    pub fn arena_bytes(&self) -> usize {
        round8(self.key_attnos.len() * 2) + round8(self.param_slots.len() * 2) + round8(self.out_attnos.len() * 2) + round8(self.lanes.len() * core::mem::size_of::<LaneDesc>()) + round8(self.zone.len() * core::mem::size_of::<ZonePred>()) + round8(self.zone_any.len() * core::mem::size_of::<ZonePred>()) + round8(self.zone_any_ends.len() * 2) + (self.driving_lanes.words().len() + self.filter_lanes.words().len() + self.answer_lanes.words().len()) * 8 + round8(self.tables.len() * core::mem::size_of::<TableDecl>()) + round8(self.table_bytes.len()) + round8(self.shreds.len() * core::mem::size_of::<ShredDecl>()) + round8(self.set.len() * 4)
    }

    /// The attributes-only lane list: one width-8 by-value lane per out attno, every one driving.
    fn attribute_lanes(mut self, out_attnos: &[i16]) -> SiteDesc {
        for (k, &a) in out_attnos.iter().enumerate() {
            self.lanes.push(LaneDesc::attr(a));
            self.driving_lanes.insert(k);
        }
        self
    }

    /// Append a derived lane (offset, rowid, verdict, projection, jsonb path) after the attributes.
    pub fn with_lane(mut self, l: LaneDesc, driving: bool) -> SiteDesc {
        self.add_lane(l, driving);
        self
    }

    pub fn add_lane(&mut self, l: LaneDesc, driving: bool) {
        if driving {
            self.driving_lanes.insert(self.lanes.len());
        }
        self.lanes.push(l);
    }

    pub fn with_zone(mut self, z: ZonePred) -> SiteDesc {
        self.add_zone(z);
        self
    }

    pub fn add_zone(&mut self, z: ZonePred) {
        assert!((z.lane as usize) < self.lanes.len(), "zone predicate");
        self.zone.push(z);
    }

    /// D3009: add a disjunction (`zone_any`), one predicate per arm; a group already there is not added again.
    pub fn add_zone_any(&mut self, arms: &[ZonePred]) {
        assert!(arms.len() >= 2 && arms.iter().all(|z| (z.lane as usize) < self.lanes.len()), "zone disjunction");
        if self.zone_any_groups().any(|g| g == arms) {
            return;
        }
        self.zone_any.extend_from_slice(arms);
        self.zone_any_ends.push(self.zone_any.len() as u16);
    }

    /// D3009: the disjunctions, each as its arms' predicates.
    pub fn zone_any_groups(&self) -> impl Iterator<Item = &[ZonePred]> {
        self.zone_any_ends.iter().scan(0usize, |lo, &hi| {
            let g = &self.zone_any[*lo..hi as usize];
            *lo = hi as usize;
            Some(g)
        })
    }

    /// Lane `k` is read only by the zone predicates: undecoded in an `AllMatch` window.
    pub fn filter_only(mut self, k: usize) -> SiteDesc {
        self.set_filter_only(k);
        self
    }

    pub fn set_filter_only(&mut self, k: usize) {
        assert!(k < self.lanes.len(), "filter-only lane");
        self.filter_lanes.insert(k);
    }

    /// Attribute lane `k` is a dictionary text column published as its code lane (`LANE_CODES`, width 4, the part's dictionary; §1.10 increment 3 step 4).
    pub fn codes_lane(mut self, k: usize) -> SiteDesc {
        self.set_codes_lane(k);
        self
    }

    /// Table `decl` (the same declaration once) as the next table lane after the attribute and offset lanes.
    pub fn declare_table(&mut self, decl: TableDecl) -> Option<u16> {
        if let Some(i) = self.tables.iter().position(|t| *t == decl) {
            return Some(i as u16);
        }
        let i = self.tables.len();
        if self.lanes.len() != self.natts() + 1 + i {
            return None;
        }
        self.tables.push(decl);
        self.add_lane(LaneDesc::table(i as u16), false);
        if decl.is_lane_fingerprint() {
            if let Some(Rep::Code { dict }) = self.lanes.get_mut(decl.over as usize).map(|l| &mut l.rep) {
                dict.fp_table = i as u8;
            }
        }
        Some(i as u16)
    }

    /// M6 D2 (L0-g): jsonb column `attno`'s shredded path of root key `path` at `kind` (`shred_kind`) and `scale` as an attribute lane of
    /// this range site (the same declaration once): inserted after the attribute lanes, before the offset lane, so it is
    /// declared before any table or derived lane. `None` for any other site or once a later lane is declared.
    pub fn add_shred(&mut self, attno: i16, path: &[u8], kind: u8, scale: i16) -> Option<usize> {
        let SiteKind::RangeSource { natts, nkeys, zkeys } = self.kind else { return None };
        let n = natts as usize;
        if let Some(d) = self.shreds.iter().find(|d| d.kind == kind && d.scale == scale && self.out_attnos[d.lane as usize] == attno && self.table_arg_at(d.path) == path) {
            return Some(d.lane as usize);
        }
        if self.lanes.len() != n + 1 || self.lanes[n].id != LaneId::Offset || !self.tables.is_empty() || n >= u8::MAX as usize {
            return None;
        }
        let mut l = LaneDesc::value(LaneId::JsonbPath(self.shreds.len() as u16), lane::KIND_BYREF, 8, lane::REGION_ARENA);
        match kind {
            shred_kind::TEXT => {
                l.rep = Rep::VarlenaPtr;
                l.life = LaneLife::PGRC;
            }
            shred_kind::TEXT_CODES => {}
            // P1b: a number path sealed at scale 0 is an int8 word per row (hosts `ShredLane::lane_of`)
            shred_kind::NUMERIC_FS if scale == 0 => {
                l.kind = lane::KIND_BYVAL;
                l.rep = Rep::Word { bytes: 8 };
            }
            shred_kind::PRESENCE => l = LaneDesc::value(LaneId::JsonbPath(self.shreds.len() as u16), lane::KIND_BYVAL, 8, lane::REGION_ARENA),
            shred_kind::OCTET_LENGTH | shred_kind::NONEMPTY | shred_kind::CHAR_LENGTH => l = LaneDesc::value(LaneId::JsonbPath(self.shreds.len() as u16), lane::KIND_BYVAL, 8, lane::REGION_ARENA),
            _ => return None,
        }
        let path = self.table_bytes_of(path);
        self.shreds.push(ShredDecl { lane: n as u8, kind, scale, path, claims: 0 });
        self.lanes.insert(n, l);
        self.out_attnos.push(attno);
        self.driving_lanes.insert(n);
        self.kind = SiteKind::RangeSource { natts: natts + 1, nkeys, zkeys };
        if kind == shred_kind::TEXT_CODES {
            self.set_codes_lane(n);
        }
        Some(n)
    }

    /// D3038 (R4): column lane `at` of this range site as its own length lane (`shred_kind::OCTET_LENGTH` or
    /// `NONEMPTY`) in place: the same index and out attno, so every lane index the decomposer recorded still names its
    /// lane; the column itself is no longer read (no code, no dictionary). `None` for any other site, lane or kind, or once
    /// a table is declared.
    pub fn shred_in_place(&mut self, at: usize, kind: u8) -> Option<usize> {
        let SiteKind::RangeSource { natts, .. } = self.kind else { return None };
        let column = matches!(self.lanes.get(at).map(|l| l.id), Some(LaneId::Attr(a)) if a > 0);
        if at >= natts as usize || at > u8::MAX as usize || !column || !matches!(kind, shred_kind::OCTET_LENGTH | shred_kind::NONEMPTY | shred_kind::CHAR_LENGTH) || !self.tables.is_empty() || self.entries.is_some() {
            return None;
        }
        let path = self.table_bytes_of(&[]);
        self.lanes[at] = LaneDesc::value(LaneId::JsonbPath(self.shreds.len() as u16), lane::KIND_BYVAL, 8, lane::REGION_ARENA);
        self.shreds.push(ShredDecl { lane: at as u8, kind, scale: 0, path, claims: 0 });
        self.driving_lanes.insert(at);
        self.filter_lanes.remove(at);
        self.answer_lanes.remove(at);
        Some(at)
    }

    /// The `table_bytes` slice `(offset, length)`.
    pub fn table_arg_at(&self, (o, n): (u32, u32)) -> &[u8] {
        &self.table_bytes[o as usize..(o + n) as usize]
    }

    /// M6 D2: the attribute lanes (bit l) a filter reads only through their path lanes' claimed compares, `reads[l]` its
    /// reads of lane l: each is late, its image filled where a path lane is invalid (hosts `with_late_guard`), for the
    /// rows the compares walk, and for the survivors.
    pub fn path_only(&self, reads: &[u32]) -> u64 {
        let mut m = 0u64;
        for (l, &r) in reads.iter().enumerate().take(64) {
            let LaneId::Attr(a) = self.lanes[l].id else { continue };
            let claims: u32 = self.shreds.iter().filter(|d| self.out_attnos.get(d.lane as usize) == Some(&a)).map(|d| d.claims as u32).sum();
            if claims > 0 && claims == r {
                m |= 1 << l;
            }
        }
        m
    }

    /// The shred declaration of lane `k`, if it is one.
    pub fn shred_of(&self, k: usize) -> Option<&ShredDecl> {
        self.shreds.iter().find(|d| d.lane as usize == k)
    }

    pub fn table_arg(&self, t: &TableDecl, k: usize) -> &[u8] {
        let (o, n) = t.args[k];
        &self.table_bytes[o as usize..(o + n) as usize]
    }

    pub fn table_bytes_of(&mut self, b: &[u8]) -> (u32, u32) {
        let o = self.table_bytes.len() as u32;
        self.table_bytes.extend_from_slice(b);
        (o, b.len() as u32)
    }

    pub fn set_codes_lane(&mut self, k: usize) {
        assert!(k < self.lanes.len(), "codes lane");
        let l = &mut self.lanes[k];
        l.kind = lane::KIND_CODES;
        l.width = 4;
        l.region = lane::REGION_DICTIONARY;
        l.rep = Rep::Code { dict: DictRef::lane(k as u8) };
        l.life = LaneLife::DICTIONARY;
    }

    /// Whether a lane of the site is a dictionary code lane (claims then run whole parts).
    pub fn has_codes(&self) -> bool {
        self.lanes().iter().any(|l| l.kind == lane::KIND_CODES)
    }

    /// Lane `k`'s aggregate may come from the answered window's record (nonnull / sum / min / max).
    pub fn answered(mut self, k: usize) -> SiteDesc {
        self.set_answered(k);
        self
    }

    pub fn set_answered(&mut self, k: usize) {
        assert!(k < self.lanes.len(), "answer lane");
        self.answer_lanes.insert(k);
    }

    /// The code lanes' dictionaries are probed, not streamed (`sparse_dicts`).
    pub const fn sparse_dicts(mut self) -> SiteDesc {
        self.sparse_dicts = true;
        self
    }

    /// The code lanes' dictionary heads are prefaulted per part (`head_prefault`).
    /// A heap range published as raw pages (`raw_pages`).
    pub const fn raw_pages(mut self) -> SiteDesc {
        self.raw_pages = true;
        self
    }

    /// A range site over the table AM's rows (`rows_via_am`).
    pub const fn rows_via_am(mut self) -> SiteDesc {
        self.rows_via_am = true;
        self
    }

    pub const fn head_prefault(mut self) -> SiteDesc {
        self.head_prefault = true;
        self
    }

    /// Code lane `k` is published as its parts' dictionary entries (`entries`): the site's only attribute lane.
    pub fn entries_lane(mut self, k: usize) -> SiteDesc {
        assert!(self.natts() == 1 && k == 0 && self.lanes[k].kind == lane::KIND_CODES, "the entries lane is the site's one attribute, a code lane");
        self.entries = Some(k as u8);
        self
    }

    pub fn lanes(&self) -> &[LaneDesc] {
        &self.lanes
    }

    pub fn zone_preds(&self) -> &[ZonePred] {
        &self.zone
    }

    /// Drop every zone predicate (the site's quals are answered another way).
    pub fn clear_zone(&mut self) {
        self.zone.clear();
    }

    pub fn nlanes(&self) -> usize {
        self.lanes.len()
    }

    pub fn lane_index(&self, id: LaneId) -> Option<usize> {
        self.lanes().iter().position(|l| l.id == id)
    }

    /// sortmerge: the site merges its parts' sorted streams on `merge` (decompose's mark; see the field).
    pub fn is_merged(&self) -> bool {
        !self.merge.is_empty()
    }

    /// sortmerge: the merge key `keys` (attnos in key order; empty: none). False, the site unchanged: more than
    /// `limits::KEYS` attnos.
    #[must_use]
    pub fn set_merge(&mut self, keys: &[i16]) -> bool {
        MergeAttnos::of(keys).map(|m| self.merge = m).is_some()
    }

    pub fn index_source(a: ArenaRef, relation: u32, index: u32, key_attnos: &[i16], param_slots: &[i16], out_attnos: &[i16]) -> SiteDesc {
        assert!(param_slots.len() == key_attnos.len(), "site shape");
        let d = SiteDesc { kind: SiteKind::IndexSource { nkeys: key_attnos.len() as u16, natts: out_attnos.len() as u16, worker: false, leaves: false, ranges: false }, relation, index, key_attnos: copy_vec(key_attnos, a), param_slots: copy_vec(param_slots, a), out_attnos: copy_vec(out_attnos, a), ..SiteDesc::none_in(a) };
        d.attribute_lanes(out_attnos)
    }

    pub const fn write(relation: u32, natts: u16) -> SiteDesc {
        let a = ArenaRef::EMPTY;
        SiteDesc { kind: SiteKind::Write { natts }, relation, index: 0, key_attnos: ArenaVec::new_in(a), param_slots: ArenaVec::new_in(a), out_attnos: ArenaVec::new_in(a), lanes: ArenaVec::new_in(a), driving_lanes: LaneSet::new_in(a), zone: ArenaVec::new_in(a), zone_any: ArenaVec::new_in(a), zone_any_ends: ArenaVec::new_in(a), filter_lanes: LaneSet::new_in(a), answer_lanes: LaneSet::new_in(a), sparse_dicts: false, dict_renders: 0, head_prefault: false, raw_pages: false, rows_via_am: false, entries: None, entries_filtered: false, entries_nulls: false, entries_desc: false, tables: ArenaVec::new_in(a), table_bytes: ArenaVec::new_in(a), shreds: ArenaVec::new_in(a), walk: None, set: ArenaVec::new_in(a), set_prune: None, merge: MergeAttnos::NONE }
    }

    pub fn words(a: ArenaRef, n: u16) -> SiteDesc {
        SiteDesc { kind: SiteKind::Words { n }, ..SiteDesc::none_in(a) }
    }

    /// D732: the COPY mark site of the scan of range table index `scanrelid` (kept in `index`).
    pub fn copy(a: ArenaRef, scanrelid: u32) -> SiteDesc {
        SiteDesc { kind: SiteKind::Copy, index: scanrelid, ..SiteDesc::none_in(a) }
    }

    /// D776: the marked-row site of source site `of` (relation `relation`).
    pub fn row_of(a: ArenaRef, of: u16, relation: u32) -> SiteDesc {
        SiteDesc { kind: SiteKind::RowOf { of }, relation, ..SiteDesc::none_in(a) }
    }

    pub fn range_source(a: ArenaRef, relation: u32, out_attnos: &[i16]) -> SiteDesc {
        let d = SiteDesc { kind: SiteKind::RangeSource { natts: out_attnos.len() as u16, nkeys: RANGE_KEYS as u16, zkeys: 0 }, relation, out_attnos: copy_vec(out_attnos, a), ..SiteDesc::none_in(a) };
        d.attribute_lanes(out_attnos).with_lane(LaneDesc::offset(), false)
    }

    pub const fn natts(&self) -> usize {
        match self.kind {
            SiteKind::IndexSource { natts, .. } | SiteKind::RangeSource { natts, .. } | SiteKind::Write { natts } => natts as usize,
            SiteKind::Words { .. } | SiteKind::Copy | SiteKind::RowOf { .. } | SiteKind::None => 0,
        }
    }

    pub const fn nkeys(&self) -> usize {
        match self.kind {
            SiteKind::IndexSource { nkeys, .. } => nkeys as usize,
            SiteKind::RangeSource { nkeys, .. } => nkeys as usize,
            SiteKind::Words { n } => n as usize,
            _ => 0,
        }
    }

    pub const fn is_source(&self) -> bool {
        matches!(self.kind, SiteKind::IndexSource { .. } | SiteKind::RangeSource { .. })
    }

    /// A site whose position a node keeps (`Node::positions`) for a recheck: a source's, or a COPY mark's row (D732).
    pub const fn is_positioned(&self) -> bool {
        self.is_source() || matches!(self.kind, SiteKind::Copy)
    }

    pub const fn is_range(&self) -> bool {
        matches!(self.kind, SiteKind::RangeSource { .. })
    }

    /// D3014O: an index site a worker probes (`SiteKind::IndexSource { worker }`): the driver binds it on a worker
    /// instead of trapping.
    pub const fn worker_probe(&self) -> bool {
        matches!(self.kind, SiteKind::IndexSource { worker: true, .. })
    }

    /// An index-only site the pool reads by leaf pages (`SiteKind::IndexSource { leaves }`): its pipeline may run wide
    /// and a worker binds the site's leaf source (driver bind, hosts index_leaves.rs).
    pub const fn leaves(&self) -> bool {
        matches!(self.kind, SiteKind::IndexSource { leaves: true, .. })
    }

    /// An ordered index site the pool reads in key ranges (`SiteKind::IndexSource { ranges }`): its pipeline may run wide,
    /// a worker binds the site's range source, and the claim rides in its last `RANGE_KEYS` key cells (driver bind, hosts
    /// index_ranges.rs).
    pub const fn ranges(&self) -> bool {
        matches!(self.kind, SiteKind::IndexSource { ranges: true, .. })
    }

    /// The key words of an index site, without a ranges site's claim cells (`ranges`).
    pub const fn key_words(&self) -> usize {
        match self.kind {
            SiteKind::IndexSource { nkeys, ranges: true, .. } => nkeys as usize - RANGE_KEYS,
            _ => self.nkeys(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OutCol {
    pub name: [u8; NAME_LEN],
    pub typid: u32,
    pub typmod: i32,
    pub typlen: i16,
    pub typbyval: bool,
    pub format: i16,
    pub origtbl: u32,
    pub origcol: i16,
    /// The output expression's collation (`exprCollation`): the row descriptor's `attcollation`, which CREATE TABLE AS /
    /// SELECT INTO record on the created column (0 = none, PostgreSQL's InvalidOid).
    pub collation: u32,
    /// How the output cell's bytes are stored (default: the type's own storage).
    pub rep: Rep,
}

impl OutCol {
    pub fn new(name: &str, typid: u32, typmod: i32, typlen: i16, typbyval: bool) -> OutCol {
        assert!(name.len() < NAME_LEN, "column name too long");
        let mut n = [0u8; NAME_LEN];
        n[..name.len()].copy_from_slice(name.as_bytes());
        OutCol { name: n, typid, typmod, typlen, typbyval, format: 0, origtbl: 0, origcol: 0, collation: 0, rep: Rep::of_storage(typlen, typbyval) }
    }

    pub const fn origin(mut self, origtbl: u32, origcol: i16) -> OutCol {
        self.origtbl = origtbl;
        self.origcol = origcol;
        self
    }

    pub const fn collation(mut self, collation: u32) -> OutCol {
        self.collation = collation;
        self
    }

    pub fn name_bytes(&self) -> &[u8] {
        let n = self.name.iter().position(|&b| b == 0).unwrap_or(NAME_LEN);
        &self.name[..n]
    }
}
