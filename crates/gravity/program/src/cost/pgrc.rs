//! The sizes the pgrc columnar source and its lowering run by: caches, buffers and per-site or per-relation caps.
//! None of them changes an answer; each bounds memory or work. A CHOSEN value's note says why it was picked and what a
//! measurement would weigh; a FITTED one is the cost home's break-even rule (`fit`) over unit costs measured on dev data.
use cost_profile::device::Device;
use super::anchor::{self, Anchor, Ratios};
use super::fit::{break_even_div, Fitted};

/// lowerv4 ops/scan/pgrc/strategies/jsonb_inplace.rs (D564): the most elements of a constant `= ANY` / `<> ALL` text list
/// whose array the jsonb.inplace claim decodes into frame words once per call (1 + 2n words); a longer list keeps the
/// per-row host walk. CHOSEN, not measured: it covers every list PostgreSQL itself walks linearly (it hashes a constant
/// array of MIN_ARRAY_SIZE_FOR_HASHED_SAOP = 9 or more elements, optimizer/util/clauses.c), and seven such lists (33 words
/// each) fit the claim's one-byte frame-word index.
pub const JSONB_ANY_LIST_MAX: usize = 16;
/// lowerv4 ops/scan/pgrc/strategies/jsonb_inplace.rs (STEP-12): the most keys of a constant jsonb path (`#>>` /
/// jsonb_extract_path_text's text[], or a chain of `->` ending in `->>`) the jsonb.inplace claim walks, one unrolled object
/// walk per key; a longer path keeps PostgreSQL's call. CHOSEN, not measured: it bounds the program a path unrolls, never an
/// answer (past it the call serves the same value).
pub const JSONB_PATH_MAX: usize = 4;
/// lowerv4 ops/scan/pgrc/strategies/jsonb_lanes.rs: the most jsonb path lanes (a `col ->> 'key'` code lane) one pgrc
/// site declares for its leading filters' compares and its group keys, in declaration order; a path past it is read from
/// the jsonb column. CHOSEN, not measured: each path lane is one more code lane the source fills per window, and 8 bounds
/// that work per site.
pub const JSONB_PATH_LANES_MAX: usize = 8;
/// lowerv4 ops/scan/pgrc/strategies/octet_lane.rs: the most `octet_length` lanes (a varlena column's byte length per row)
/// one pgrc site declares, in reading order; a column past it keeps the per-code length table. CHOSEN, not measured: each
/// lane is one more word lane the source fills per granule, and 8 bounds that work per site, as JSONB_PATH_LANES_MAX does.
pub const OCTET_LANES_MAX: usize = 8;

/// hosts granule_source/kept.rs (D3156): a verbatim part's code lane builds the table of the granules this execution's zone
/// verdicts keep (`text_hosts::Kept`) when they are at most 1 / KEPT_GRANULES_DIV of the part's; past that it builds the
/// whole part's, which every later statement over the part reads. CHOSEN, not measured: a kept table costs its kept rows'
/// decode and sort again at each kept set no statement built yet, the whole one all the part's rows once; at 1 / 8 the kept
/// build reads at most an eighth of the part, so even 8 distinct kept sets cost no more than one whole build.
pub const KEPT_GRANULES_DIV: u64 = 8;

/// cond_cache.rs: the least resident entry count at which the condition cache sweeps out the entries of owners that were
/// dropped; after a sweep the next one waits for twice the survivors. CHOSEN, not measured: a sweep walks every entry,
/// so a floor keeps a small cache from sweeping on each insert.
pub const COND_CACHE_SWEEP_FLOOR: usize = 64;

/// cond_cache.rs (D4607): once a part closed since the condition cache's last sweep (`pgrc_read::openpart::closed_parts`
/// moved), its next insert sweeps out closed parts' entries, at most once in this many ns. CHOSEN, not measured: a sweep
/// walks every entry, so a part cache churning parts costs at most one walk a second, and a dropped database's or table's
/// entries go within a second and one new entry.
pub const COND_CACHE_CLOSED_SWEEP_GAP_NS: u64 = 1_000_000_000;

/// prime.rs after_recovery: how long (ns) the startup process waits for the last cycle's runtime work (a dead leader's
/// section) to finish before it drops that cycle's columnar state at an in-process crash restart. CHOSEN, not measured:
/// a bound on a wait that ended at once in every witness (no group left at the pass); a stuck group cannot hold the
/// restart past it.
pub const STARTUP_SETTLE_LIMIT_NS: u64 = 10_000_000_000;

/// entry_runs.rs: the parts whose (start, end) bounds the owner's read loads into one stack batch before it reads their
/// slices. CHOSEN, not measured: 256 pairs of u32 is a 2 KiB stack array, and one batch covers the parts of most lists.
pub const ENTRY_RUN_BOUNDS_BATCH: usize = 256;

/// fetch_site.rs: the least byte growth of a fetch site's decode arena (it grows to at least twice its length). CHOSEN,
/// not measured: 64 KiB, so a fetch of a few wide rows does not reallocate per column.
pub const FETCH_ARENA_MIN_BYTES: usize = 1 << 16;

/// granule_source.rs: the shredded jsonb path lanes (one per distinct `(attno, path, kind, scale)` a list's statements
/// read) a part list's faces hold beside its columns, claimed on first use; a path past it is read from the jsonb column.
/// CHOSEN, not measured: each slot is one lazily filled lane cell per list, and 64 distinct paths per table is past every
/// statement set seen.
pub const SHRED_FACES_MAX: usize = 64;

/// granule_source.rs (lanes.rs): the least free bytes the program-table region keeps before it opens a new buffer (a new
/// buffer is at least four times this, or sixteen times the last table). CHOSEN, not measured: 64 KiB, so small tables
/// share one buffer.
pub const TABLE_REGION_MIN_BYTES: usize = 1 << 16;

/// granule_source.rs (`TableState::enter`): the entries of an unfilled adaptive call key table (`table_fn::FLAG_ADAPTIVE`)
/// read to measure its fill, evenly spaced codes (a smaller table is read whole). CHOSEN, not measured: 4096 entries
/// (64 KiB) bound the read per part, and a share measured over them is within 2 % of the table's.
pub const ADAPT_SAMPLE_ENTRIES: u32 = 4096;
/// granule_source.rs (lanes.rs): the fill share (%) of an adaptive call key table's entries at which a source's next parts
/// take the hosted build instead of the rows' fill. CHOSEN from an A/B on dev data (a synthetic 10M-row table, W0 + W8,
/// against the same binary with the key hosted every part): at 25 % md5(u) WHERE id % 4 = 0 takes the build where the
/// rows' fill costs less (4797 against 2736 ms); at 50 % every sparse cell keeps the fill (md5(u) WHERE id % 1000 = 0,
/// 62 against 3354 ms hosted) and the half-dense ones take the build (upper(s) / lower(t) WHERE id % 2 = 0, 1.02 / 1.03 of
/// hosting every part).
pub const ADAPT_HOSTED_PCT: u64 = 50;

/// granule_source.rs (late.rs): the words of one render chunk a late-materialised column's rendered values are written
/// into. CHOSEN, not measured: 8192 words (64 KiB) per chunk.
pub const RENDER_CHUNK_MIN_WORDS: usize = 1 << 13;

/// The box the FITTED unit costs below were measured on.
const MACHINE: &str = "box 45: c8g.4xlarge, Graviton 4 (Neoverse V2), 16 vCPU";
/// Their dev data (never a benchmark's statements, the census or a bar cell): `tests/cost_fit.rs`, synthetic lanes at the dev
/// seed, one 8,192-row granule of u32 (it fits L1d, as a granule's lane does), least of five takes.
const CORPUS: &str = "tests/cost_fit.rs at the dev seed (machine units, one granule)";

const fn fitted(ns: f64, anchor: Anchor) -> Fitted {
    Fitted { ns, machine: MACHINE, corpus: CORPUS, anchor }
}

/// A granule's rows per lane (pgrc's elected granule; one window).
const GRANULE_ROWS: f64 = 8192.0;

// One anchor (read) for both: RT_DIV is also the idle filter's sampling interval, which a larger share delays.

/// A lane's whole decode, per row: cost_fit's `read` (a sequential pass over the granule's words).
pub const LANE_READ: Fitted = fitted(0.0928, Anchor::Read);
/// One runtime-filter check of a granule's facts record (its range, then a binary search of the filter's key set):
/// cost_fit's `probe`.
pub const RT_CHECK: Fitted = fitted(5.5746, Anchor::Read);

/// granule_source.rs (verdicts.rs; H14, kept): the checks one measuring window of the runtime filter counts before it
/// judges the filter's cut share (`RT_DIV`). The filter is armed by measurement at run time, never by a tuned guess of
/// the join's selectivity; the window is its sample size. CHOSEN: 64 checks cost 64 `RT_CHECK` (0.36 us on the reference
/// box), under one lane's whole decode of one granule (0.76 us), so a window that idles a useless filter costs less than
/// one granule it could have cut.
pub const RT_PROBE: u32 = 64;
/// granule_source.rs (verdicts.rs; P5-17): the runtime filter keeps checking while a window of its checks cuts at least one
/// granule in `RT_DIV`, the share whose saved decodes (one lane's whole decode per cut granule, the least a cut saves) pay
/// for every check; idle, it still checks one granule in `RT_DIV`, and a cut arms it again. FITTED: `break_even_div(granule
/// decode, check)` on a host of ratios `r`, 137 on the reference box and on every host (one anchor): a window with no
/// cut idles it.
pub const fn rt_div_on(r: &Ratios) -> usize {
    break_even_div(GRANULE_ROWS * LANE_READ.on(r), RT_CHECK.on(r))
}
/// `rt_div_on` at the reference.
pub const RT_DIV: usize = rt_div_on(&Ratios::ONE);
/// `rt_div_on` on this host (the installed profile): granule_source.rs reads it once a scan.
pub fn rt_div() -> usize {
    rt_div_on(anchor::ratios())
}

/// The box H09's gather and whole-decode unit costs were measured on (the reference class).
const SELECT_MACHINE: &str = "box 87: Graviton 4 (Neoverse V2), 16 vCPU";
/// Their dev data (never a benchmark's statements, the census or a bar cell): pgrc_codec `tests/select_fit.rs` at the dev
/// seed, one 8,192-row granule of a synthetic by-value lane per encoding and width, random and frame-clustered survivors,
/// least of five takes; the hosts' own kernels on both sides (BYTE_FOR's `sel_at_framed`, any other stream's `decode_sel`
/// and a scatter; `decode_narrow` / `decode_full`).
const SELECT_CORPUS: &str = "pgrc_codec tests/select_fit.rs at the dev seed (one granule, least of five)";

/// A survivor's gather moves with the gather anchor (data-dependent positions); a frame's unpack and a whole decode, with
/// the read anchor.
const fn select_fitted(ns: f64, anchor: Anchor) -> Fitted {
    Fitted { ns, machine: SELECT_MACHINE, corpus: SELECT_CORPUS, anchor }
}

/// One gather call's fixed cost (the selection's walk set up, the stream addressed): select_fit's intercepts, 12-36 ns.
pub const GATHER_CALL: Fitted = select_fitted(25.0, Anchor::Gather);
/// A BYTE_FOR gather's cost per survivor at a power-of-two delta width (1 / 2 / 4 / 8 bytes, one plain load a value):
/// `sel_at_framed`, 0.80-0.87 ns over i16 / i32 / i64 lanes.
pub const GATHER_BYTE_FOR_ROW: Fitted = select_fitted(0.82, Anchor::Gather);
/// The same at another delta width (3 / 5 / 6 / 7 bytes, a masked 8-byte load a value): 1.00-1.06 ns.
pub const GATHER_BYTE_FOR_MASKED_ROW: Fitted = select_fitted(1.03, Anchor::Gather);
/// Any other stream's gather per survivor (`decode_sel` into dense words, then the scatter to the rows): VERBATIM 1.35,
/// FFOR 1.38-1.66, DELTA_FOR 1.45 ns.
pub const GATHER_SEL_ROW: Fitted = select_fitted(1.42, Anchor::Gather);
/// An FFOR gather's cost per frame a survivor touches (`decode_sel` unpacks the frame): 302-464 ns over 4 to 40 bits.
pub const GATHER_FFOR_FRAME: Fitted = select_fitted(370.0, Anchor::Read);
/// A DELTA_FOR gather's cost per frame a survivor touches (the frame's delta chain decoded through): 631 ns.
pub const GATHER_DELTA_FOR_FRAME: Fitted = select_fitted(631.0, Anchor::Read);
/// A BYTE_FOR whole decode at a power-of-two delta width, per row and byte of the lane: 0.0124-0.0175 ns over i16 / i32 /
/// i64 lanes (`decode_narrow` / `decode_full`, the plain widen).
pub const WHOLE_BYTE_FOR_BYTE: Fitted = select_fitted(0.0150, Anchor::Read);
/// A BYTE_FOR whole decode at another delta width, per row: 0.269 / 0.282 ns (i32 at 3 bytes, i64 at 5: the masked loads).
pub const WHOLE_BYTE_FOR_MASKED_ROW: Fitted = select_fitted(0.275, Anchor::Read);
/// A VERBATIM whole decode at the lane's own width, per row and byte of the lane (a copy): 0.0129-0.0131 ns.
pub const WHOLE_VERBATIM_BYTE: Fitted = select_fitted(0.0129, Anchor::Read);
/// An FFOR whole decode, per row: 0.366-0.488 ns over 4 to 40 bits.
pub const WHOLE_FFOR_ROW: Fitted = select_fitted(0.418, Anchor::Read);
/// A DELTA_FOR whole decode, per row: 0.690 ns.
pub const WHOLE_DELTA_FOR_ROW: Fitted = select_fitted(0.690, Anchor::Read);

/// H09 (hosts granule_source/raw.rs `gather_pays`, late.rs `late_dense`): a by-value lane's values stream, as a selective
/// window weighs gathering its survivors at their rows against decoding the lane whole (a whole decode leaves the
/// survivors at their rows as the gather does). The streams select_fit measured; any other keeps the selective decode's
/// bar (`SELECT_DIV`, one bar for every lane: gather below 1/8 of the rows), which the fit found gathers where the whole
/// decode is cheaper on every encoding but BYTE_FOR at 3 or 5 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneDecode {
    /// BYTE_FOR at delta width `delta` bytes (1..=8).
    ByteFor { delta: u8 },
    /// VERBATIM at the lane's own width. A lane read in place (no copy) costs less still: the copy's cost stands for it.
    Verbatim,
    /// FFOR (the interleaved layout): a gather unpacks every frame a survivor touches.
    Ffor,
    /// DELTA_FOR: a gather decodes every frame a survivor touches through its delta chain.
    DeltaFor,
}

impl LaneDecode {
    /// `calls` gathers of `survivors` rows in all, each call unpacking the frames its own survivors touch (`frames` in all:
    /// a frame two calls touch counts twice), on a host of ratios `r`.
    pub fn gather_ns_on(self, r: &Ratios, calls: u64, frames: u64, survivors: u64) -> f64 {
        let (row, frame) = match self {
            Self::ByteFor { delta: 1 | 2 | 4 | 8 } => (GATHER_BYTE_FOR_ROW.on(r), 0.0),
            Self::ByteFor { .. } => (GATHER_BYTE_FOR_MASKED_ROW.on(r), 0.0),
            Self::Verbatim => (GATHER_SEL_ROW.on(r), 0.0),
            Self::Ffor => (GATHER_SEL_ROW.on(r), GATHER_FFOR_FRAME.on(r)),
            Self::DeltaFor => (GATHER_SEL_ROW.on(r), GATHER_DELTA_FOR_FRAME.on(r)),
        };
        calls as f64 * GATHER_CALL.on(r) + frames as f64 * frame + survivors as f64 * row
    }

    /// The whole decode of `rows` rows into a lane of `lane_bytes` bytes a value, on a host of ratios `r`.
    pub fn whole_ns_on(self, r: &Ratios, lane_bytes: u8, rows: u64) -> f64 {
        let row = match self {
            Self::ByteFor { delta: 1 | 2 | 4 | 8 } => WHOLE_BYTE_FOR_BYTE.on(r) * lane_bytes as f64,
            Self::ByteFor { .. } => WHOLE_BYTE_FOR_MASKED_ROW.on(r),
            Self::Verbatim => WHOLE_VERBATIM_BYTE.on(r) * lane_bytes as f64,
            Self::Ffor => WHOLE_FFOR_ROW.on(r),
            Self::DeltaFor => WHOLE_DELTA_FOR_ROW.on(r),
        };
        rows as f64 * row
    }

    /// Whether one gather of `survivors` rows touching `frames` frames costs less than the whole decode of the granule's
    /// `rows` rows, on a host of ratios `r`.
    pub fn gather_pays_on(self, r: &Ratios, lane_bytes: u8, rows: u64, survivors: u64, frames: u64) -> bool {
        self.gather_ns_on(r, 1, frames, survivors) < self.whole_ns_on(r, lane_bytes, rows)
    }

    /// `gather_ns_on` on this host (the installed profile).
    pub fn gather_ns(self, calls: u64, frames: u64, survivors: u64) -> f64 {
        self.gather_ns_on(anchor::ratios(), calls, frames, survivors)
    }

    /// `whole_ns_on` on this host.
    pub fn whole_ns(self, lane_bytes: u8, rows: u64) -> f64 {
        self.whole_ns_on(anchor::ratios(), lane_bytes, rows)
    }

    /// `gather_pays_on` on this host.
    pub fn gather_pays(self, lane_bytes: u8, rows: u64, survivors: u64, frames: u64) -> bool {
        self.gather_pays_on(anchor::ratios(), lane_bytes, rows, survivors, frames)
    }
}

// KEYED-24's covered window (lowerv4 ops/group/distinct_plain.rs) takes the window host only from a bypassed table, with
// no fitted guard. Do not re-fit one on the sc1 sweep k24h2 (box 7, c8g): its survivors came in runs of equal keys
// (dM constant over 10-row blocks), so its slope (1.37 ns) was a saving per adjacent duplicate the host drops, not per
// selected row. Held out (2M rows, run-free survivors, cache on, W0; walk / host ms): table mode 4000 keys at 491 / 2458
// selected a window 2.50 / 2.59 and 5.01 / 5.73, 12000 int8 at 2048 5.08 / 5.58, 12000 int4 at 2048 4.66 / 5.53;
// bypassed 24000 at 20 / 2048 2.16 / 2.03 and 10.64 / 9.59; the same tables in runs of 10: 3.74 / 2.97, 3.68 / 3.05.

/// granule_source.rs (read_knobs; H30 / KN-05): the largest hole a claim's run read reads and discards between two extents:
/// the bytes a sequential read covers in one read's fixed cost (`Device::coalesce_gap`), so reading the hole costs no more
/// than the second read it saves. The cold regime's: a run reads only extents the page cache does not hold. FITTED by
/// cost_profile tests/device_fit.rs on the reference box: 334,567 bytes (326 KiB).
pub const READ_HOLE_BYTES: u64 = Device::COLD.coalesce_gap();

/// granule_source.rs (read_knobs; H30 / KN-05): the largest run read, in bytes: the bytes PostgreSQL's own read stream keeps
/// in flight, its combined read (`combine`, io_combine_limit in bytes) times the I/Os it issues ahead (`ahead`,
/// effective_io_concurrency), so a claim's run reads no further ahead than a heap scan would on the same settings (2 MiB at
/// PostgreSQL 18's defaults, 128 kB x 16). Measured on a cold scan (box 41, the device's own product of 1,280 KiB x 63 =
/// 79 MiB per run against 2 MiB): no faster, so the run stays at the read stream's distance.
pub const fn read_run_bytes(combine: u64, ahead: u64) -> u64 {
    combine.saturating_mul(ahead)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference box's values of the FITTED and derived sizes, so a refit or a rule that moves them is seen here first
    /// (the sources cite these numbers).
    #[test]
    fn the_reference_box_sizes() {
        assert_eq!(RT_DIV, 137);
        assert_eq!(READ_HOLE_BYTES, 334_567);
        assert_eq!(read_run_bytes(16 * 8192, 16), 2 << 20, "PostgreSQL's defaults: io_combine_limit 128 kB x effective_io_concurrency 16");
        assert_eq!(read_run_bytes(u64::MAX, 2), u64::MAX);
    }

    /// The largest survivor share 1/div (select_fit's ladder) at which the model still gathers one granule's random
    /// survivors (every frame touched), per stream and lane width; select_fit's measured crossovers in the comments.
    #[test]
    fn the_reference_box_gather_crossovers() {
        const ROWS: u64 = 8192;
        let pays_down_to = |d: LaneDecode, lane_bytes: u8| {
            [128u64, 64, 32, 24, 16, 12, 10, 8, 6, 5, 4, 3, 2].into_iter().take_while(|div| d.gather_pays(lane_bytes, ROWS, ROWS / div, 8)).last()
        };
        let bf = |delta| LaneDecode::ByteFor { delta };
        // BYTE_FOR i16 at 1 / 2 bytes: whole cheaper from 1/32 / 1/24
        assert_eq!((pays_down_to(bf(1), 2), pays_down_to(bf(2), 2)), (Some(32), Some(32)));
        // i32 at 1 / 2 / 4 bytes: from 1/16 / 1/16 / 1/12; at 3 bytes from 1/4
        assert_eq!((pays_down_to(bf(1), 4), pays_down_to(bf(4), 4), pays_down_to(bf(3), 4)), (Some(16), Some(16), Some(4)));
        // i64 at 1 / 2 / 4 / 8 bytes: from 1/6 / 1/6 / 1/8 / 1/6; at 5 bytes from 1/3
        assert_eq!((pays_down_to(bf(8), 8), pays_down_to(bf(5), 8)), (Some(8), Some(4)));
        // VERBATIM i32 / i64: from 1/24 / 1/12
        assert_eq!((pays_down_to(LaneDecode::Verbatim, 4), pays_down_to(LaneDecode::Verbatim, 8)), (Some(32), Some(16)));
        // FFOR (any width): from 1/24, i64 at 40 bits from 1/128; DELTA_FOR: from 1/16
        assert_eq!((pays_down_to(LaneDecode::Ffor, 4), pays_down_to(LaneDecode::DeltaFor, 4)), (Some(32), Some(24)));
        // survivors in one frame: FFOR and DELTA_FOR gather a quarter of a frame's rows for less than the granule's decode
        assert!(LaneDecode::Ffor.gather_pays(4, ROWS, 256, 1) && LaneDecode::DeltaFor.gather_pays(4, ROWS, 256, 1));
        // the late fills' sum: 16 calls of 64 survivors, each unpacking its own frame, against 1,024 rows decoded whole
        assert!(LaneDecode::Ffor.gather_ns(16, 16, 1024) > LaneDecode::Ffor.whole_ns(4, 1024));
    }
}

/// hosts merged.rs (sortmerge, D4469 road 2): the bytes a merged range site's live windows hold at most (B_live), each live
/// part stream one decoded granule of the site's lanes. A merge over more parts than fit keeps the rest as a head key and
/// opens their granule again when each reaches the top. CHOSEN, not measured: a starting value, to be re-cut by a 200-part
/// timing.
pub const MERGE_LIVE_BYTES: usize = 64 << 20;
/// hosts merged.rs: the most live windows one merged site keeps, whatever its lanes' width (each holds one granule source
/// in the Node's arena, reserved at its build). CHOSEN, not measured: K_PLAN (256), the most parts a planner's merged path
/// is built for, so a narrow site holds every part of such a plan live.
pub const MERGE_LIVE_MAX: usize = 256;
/// hosts merged.rs: the bytes one live window counts per lane of its site (a granule of 8-byte cells, its validity and a
/// by-reference lane's first arena) when B_live is cut into windows. INFERRED from granule_source.rs's bind sizes (natts x
/// (ROWS + PF_WORDS) words, VALIDITY_WORDS words, ARENA_FIRST bytes), not measured.
pub const MERGE_LANE_BYTES: usize = 136 << 10;
