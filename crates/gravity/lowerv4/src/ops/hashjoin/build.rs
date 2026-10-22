//! Hash build (H13 H18 H23), the producer side of `join.inner`: the build pipeline's end appends one record per row with a
//! non-NULL key to its own region, and the merge (`BuildMerge`, the driver's `Merge` as `MergeImpl`) appends the workers'
//! records into the leader's region and lays the directory out once every row is in: a KeyHash directory at <= 1/2 load
//! (sparser for a small build: the cost home's loads, rt.rs `dir_bits`) with LIFO chains and the bloom tags in its heads
//! (bloomtag.rs), or a dense table over the observed key range (dense.rs). Duplicate keys are always handled; uniqueness is only ever proven at run time (a dense table that
//! meets a duplicate chains). A build past its share grows (D254: region_grow in the end, State::grow in the merge), never a
//! spill the legacy tables would misread (the region's kind is 7).
//! Region: [header HDR | records n x rec | marks | directory ... | ... by-reference images, from the region's end down].
//! Record: [image words | link (low 32: next record + 1, high 32: payload NULL bits) | payload words].
//! L0-d (M7): a build its probe fills the inner of (RIGHT / FULL / RIGHT ANTI, PG's keep_nulls = HJ_FILL_INNER) keeps a
//! NULL-key row as a record no directory chains (image words 0, link low half `NULLKEY` until the lay-out moves it to the
//! records' tail `[N, N + KEPT)`); a build its probe marks (decompose `marks: true`) lays one mark byte per record after
//! the records (`MARKS`) and only the KeyHash form, so a match always has its record's address.
//! The region's runtime (merge, grow, lay-out) is build_rt.rs. Budget 400 lines. Owner M1-JOIN.
use std::rc::Rc;

use gravity_abi::breaker::{Breaker, BreakerKind, Spill};
use gravity_abi::ctx::{hdr, partials};
use gravity_abi::program::prog;
use gravity_abi::status::raise;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::spill::{self, Holds, NoSpill};
use gravity_program::{ExprOut, LifeId, Pipeline, Rep, Until};
use types_nodes::jointype::JoinType;
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

use crate::cell::{At, Cell, Nullable};
use crate::face::end::EndMech;
use crate::ops::hashjoin::face::Swap;
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::frame::frame::EndMaker;
use crate::frame::px::{ExId, Px};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::rep::{self, CellRef, KeepSpace, Lx, RetainHome};

use crate::ops::hashjoin::build_class::{defers, outer};
use crate::ops::hashjoin::build_side::{shape, Shape};
use crate::ops::hashjoin::keys::{any_null, check_keys, image, key_cells, key_src, KeySrc};
use crate::ops::hashjoin::raw::build_rows as rows;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::JoinInner, &EndMaker(make))], ..crate::ops::NONE };

/// The header's bytes; records start here (a cache line apart). The driver zeroes the first 64 and arms `LIMIT`.
pub(crate) const HDR: usize = 192;
/// `TABLE_V4BUILD` (abi breaker.rs, byte 15): what the region holds.
pub(crate) const KIND: u8 = 7;
pub(crate) const PHASE: usize = 0; // u8: 0 fresh, 1 rows, 2 laid out
pub(crate) const FLAGS: usize = 1; // u8 F_*
pub(crate) const WORDS: usize = 2; // u8 image words
pub(crate) const ALLOW: usize = 3; // u8 A_*: the forms this program's probe reads
pub(crate) const REC: usize = 4; // u32 record bytes
pub(crate) const LIMIT: usize = core::mem::offset_of!(gravity_abi::breaker::HashAgg, limit); // u32 region bytes, the word the driver arms (HashAgg::arm)
pub(crate) const FORM: usize = 12; // u8
pub(crate) const PAY: usize = 13; // u8 payload words
pub(crate) const VBITS: usize = 14; // u8 a compact record's payload value bits, stamped when not a bool's 1 (dense_bits.rs)
pub(crate) const N: usize = 16; // u64 records
pub(crate) const LO: usize = 24; // i64 least image word (one-word images)
pub(crate) const HI: usize = 32; // i64 greatest
pub(crate) const DIR: usize = 40; // u64 directory offset
pub(crate) const BITS: usize = 48; // u32 directory bits
/// USEROP's one-batch guard (ops/shared/one_batch.rs): PostgreSQL's tuple bound, 0 for none (in the 64 bytes the driver
/// zeroes), and log2 of its first bucket count; the lay-out leaves past them by a run-time refusal.
pub(crate) const GUARD: usize = 52; // u16
pub(crate) const GUARD_BUCKETS: usize = 54; // u8
pub(crate) const TOP: usize = 56; // u64 the images' low end
pub(crate) const SLOTS: usize = 64; // u64 dense slots offset
pub(crate) const END: usize = 80; // u64 the region's bytes (images are kept by their distance from it until the lay-out)
pub(crate) const DESC: usize = 128; // [u16; 32] payload image kinds: 0 a word, D_VARLENA, D_CSTR, else fixed bytes
pub(crate) const EMPTY: u8 = 0;
pub(crate) const HASH: u8 = 1;
pub(crate) const DENSE: u8 = 2;
pub(crate) const DCHAIN: u8 = 3;
pub(crate) const DBITS: u8 = 5; // dense_bits.rs (4 is no form)
/// Op 14: the build raised and its merge held the error for the probe's first row (build_rt.rs `defer_error`).
pub(crate) const FAILED: u8 = 0xFF;
pub(crate) const FULL: usize = 88; // u32 a compact build's full record bytes (dense_bits.rs `expand`)
pub(crate) const KEPT: usize = 96; // u64 NULL-key records (L0-d): of the N before the lay-out, after it past the N keyed ones
pub(crate) const MARKS: usize = 104; // u64 the marks array's offset (L0-d), laid out with the directory
/// D3194 (join.rowfilter): log2 of the Bloom words laid past the KeyHash directory (strategies/rowfilter.rs), 0 for none;
/// every lay-out of the HASH form writes it (bytes 112..120 hold the spill words SPILL / PBITS / SPILLED around it).
pub(crate) const RF_BITS: usize = 114; // u8
pub(crate) const F_COMPACT: u8 = 4;
pub(crate) const F_RANGE: u8 = 1;
pub(crate) const F_DUP: u8 = 2;
/// L0-d: the lay-out moved the NULL-key records to the tail; `N` counts the keyed ones, `KEPT` the rest.
pub(crate) const F_SPLIT: u8 = 8;
/// AGA-08: `LO` / `HI` hold the first key part's range in a key of several image words whose first part is one word (the
/// run-time filter's range over that column, rtfilter.rs); `F_RANGE` stays a one-word key's (the dense and list forms').
pub(crate) const F_RANGE0: u8 = 16;
/// PS1-22: a one-key build whose key is a text fingerprint (rtfilter.rs `text_fp_key`: two `Fp128` words, rule A, a bytes-equal
/// collation), stamped by the build and kept by the merges only when every partial holds it: join.rtfilter exports its keys
/// as fingerprints (16 is hashjoin's AGA-08 `F_RANGE0`).
pub(crate) const F_FP128: u8 = 32;
/// join.dedup (strategies/dedup.rs): the probe's end keeps one copy of a joined row, so the lay-out chains one record of
/// each set of equal ones (stamped by the build; the merge is the serial one, build_part.rs `plan`).
pub(crate) const F_DEDUP: u8 = 64;
/// D4904 (strategies/dedup.rs `keys`): a SEMI / ANTI probe reads this build for its keys alone, so a record whose image words
/// another holds may go before the build grows or spills (`compact`, called from spill/raw/region.rs).
pub(crate) const F_KEYS: u8 = 128;
pub(crate) const A_DENSE: u8 = 1;
pub(crate) const A_TAGS: u8 = 2;
/// join.mult: the probe reads each key's count, so equal keys collapse into one record (multiplicity.rs).
pub(crate) const A_MULT: u8 = 8;
/// L0-d: the probe fills the inner (RIGHT / FULL / RIGHT ANTI), so NULL-key rows are kept; the probe marks its matches.
pub(crate) const A_KEEP: u8 = 16;
pub(crate) const A_MARKS: u8 = 32;
/// nest.eqkey (decompose self_spool.rs `keyed`): a catalog probe reads a key's records in record order (the Nest's order
/// within an outer row), so the lay-out chains each key's records first to last (bloomtag.rs `lay`, dense.rs DCHAIN) and
/// the merge is the serial one (build_part.rs `plan`).
pub(crate) const A_FIFO: u8 = 64;
/// D3194 (join.rowfilter): the probe tests the build's Bloom words ahead of its directory, so a HASH lay-out lays them
/// (strategies/rowfilter.rs).
pub(crate) const A_RF: u8 = 128;
/// A kept NULL-key record's link low half until the lay-out (every other record's is 0 there).
pub(crate) const NULLKEY: u64 = LOW;
/// A directory head's record bits (idx + 1); the bloom tags sit above them.
pub(crate) const IDX: u64 = (1 << 48) - 1;
pub(crate) const LOW: u64 = u32::MAX as u64;
pub(crate) const D_VARLENA: u16 = u16::MAX;
pub(crate) const D_CSTR: u16 = u16::MAX - 1;

/// The region of this pipeline (`None`) or of its consumed slot at position `i` of `Pipeline::consumes`.
pub(crate) fn region(b: &mut FunctionBuilder<'_>, env: &Env, consumed: Option<usize>) -> Value {
    let pb = b.load(Type::Ptr, env.ctx, hdr::PROGRAM as i32);
    let pp = b.load(Type::Ptr, pb, prog::PARTIAL as i32);
    match consumed {
        None => b.load(Type::Ptr, pp, partials::OWN as i32),
        Some(i) if i < 2 => b.load(Type::Ptr, pp, (partials::CONSUMED + 8 * i) as i32),
        Some(i) => {
            let m = b.load(Type::Ptr, pp, partials::MORE as i32);
            b.load(Type::Ptr, m, (8 * i) as i32)
        }
    }
}

/// L0-d: mark byte 0's address (`I64`) of the build whose region's address is `base` (`I64`; `MARKS` is its offset).
#[allow(dead_code)] // M7-PROBE's walk (B1) and join/drain_src.rs (M7-DRAIN B2) call it
pub(crate) fn marks_base(b: &mut FunctionBuilder<'_>, base: Value) -> Value {
    let p = b.conv(ConvOp::IntToPtr, Type::Ptr, base);
    let off = b.load(Type::I64, p, MARKS as i32);
    b.int_bin(IntBinOp::Add, base, off)
}

/// L0-d: the mark byte (`Ptr`) of the record at `rec`, records of `size` bytes from `recs` (each `I64` or `Ptr`).
fn mark_at(b: &mut FunctionBuilder<'_>, marks: Value, recs: Value, rec: Value, size: u32) -> Value {
    let (rec, recs, marks) = (b.conv(ConvOp::PtrToInt, Type::I64, rec), b.conv(ConvOp::PtrToInt, Type::I64, recs), b.conv(ConvOp::PtrToInt, Type::I64, marks));
    let (off, size) = (b.int_bin(IntBinOp::Sub, rec, recs), b.iconst(Type::I64, size as i64));
    let i = b.int_bin(IntBinOp::Udiv, off, size);
    let a = b.int_bin(IntBinOp::Add, marks, i);
    b.conv(ConvOp::IntToPtr, Type::Ptr, a)
}

/// L0-d: marks the record at `rec` matched (the straight form: a plain store; every worker stores the same byte).
#[allow(dead_code)] // M7-PROBE's walk (B1)
pub(crate) fn mark(b: &mut FunctionBuilder<'_>, marks: Value, recs: Value, rec: Value, size: u32) {
    let (p, one) = (mark_at(b, marks, recs, rec, size), b.iconst(Type::I8, 1));
    b.store(p, 0, one);
}

/// L0-d: whether the record at `rec` is marked (`Bool`): RIGHT SEMI's test before its join qual (walk.rs `hand_on`, D2003).
pub(crate) fn marked(b: &mut FunctionBuilder<'_>, marks: Value, recs: Value, rec: Value, size: u32) -> Value {
    let p = mark_at(b, marks, recs, rec, size);
    let (v, z) = (b.load_zext(Type::I8, p, 0), b.iconst(Type::I64, 0));
    b.icmp(IntCond::Ne, v, z)
}

struct BuildEnd<'g> {
    sh: Rc<Shape>,
    facts: &'g dyn gravity_decompose::catalog::Facts,
    keys: PgVec<'g, KeySrc>,
    node: i32,
    /// A' (hoist.rs): the payload column that keeps a call's verdict in place of its code, the call and its value form.
    hoist: Option<(i16, ExId, ExprOut)>,
    compact: core::cell::Cell<bool>,
    /// AGA-08: the image's first key part is one word of several (`F_RANGE0`: `LO` / `HI` track it).
    range0: core::cell::Cell<bool>,
    /// L0-d: NULL-key rows kept (`A_KEEP`, one more carried count: `KEPT`); the probe marks (`A_MARKS`).
    keep: bool,
    marks: bool,
    /// M7-SPILL (V4R-359): the partition bits of a build that may spill (join/build_spill.rs; 0: it grows by name, D254).
    spill: u8,
    /// C10: why a build of no partition bits grows past its grant (`spill::NoSpill`); `None` when it may spill.
    why: Option<NoSpill>,
    /// The build's breaker, as its descriptor declares it.
    br: Breaker,
    /// B4d: the end is an arm of a build's Arms run (two or more pipelines append the records).
    arms: bool,
    /// D2854: of a foreign run (`Run::Foreign`), whose arms render a code payload to its image. D4054: a pair run's two
    /// arms read one site (`P'`'s frame site is `q`'s), so both keep the code word.
    foreign: bool,
    /// USEROP's one-batch guard (`GUARD`): a guarded build never spills, so its lay-out counts every record.
    guard: Option<(u16, u8)>,
    /// PS1-22: the build's one key is a text fingerprint (`F_FP128`).
    fp128: bool,
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let (Some(Op::Join(JoinDesc::Build { keys, decls, .. })), Some(br)) = (p.ops.last(), crate::frame::iface::breaker_of(&p.desc)) else {
        crate::refuse!(node, Reason::Bug { payload: "a hash build end without its build op" })
    };
    check_keys(px.gx.facts, node, decls, keys.len());
    let mcx = px.mcx();
    let keys = keys.iter().zip(decls.iter()).map(|(n, d)| key_src(px, n, node, d)).collect_in(mcx);
    // B4d (V4R-476): an arm of a build's Arms run other than its last appends records only: the run's last arm (the life
    // the probe reads) owns the shape, the empty-build reading (D276) and the rtfilter. D4054: of a pair run, `P'`'s copy
    let role = run_of(px, k, br.slot);
    let sh = shape(px, br.slot, role.last(k));
    let (run, cont, foreign) = (role != Run::Lone, role.continues(), matches!(role, Run::Foreign { .. }));
    // D276: arg bits 8..47 the pipelines an empty build lets the runner skip (HOSTS' BuildSideLive, read[0] its reading)
    let (mut proto, mut defer) = (Proto::new(ProtoKind::BuildSide, k as u16).slot(br.slot).reading(0, rows), false);
    let (mut keep, mut marks, mut spill) = (false, false, Err(NoSpill::BuildNoProbe));
    if let Some(q) = sh.probe {
        if let Some(Op::Join(JoinDesc::Probe { jointype, marks: m, .. })) = gx.pipelines()[q].ops.iter().find(|op| matches!(op, Op::Join(JoinDesc::Probe { slot, .. }) if *slot == br.slot)) {
            (keep, marks) = (matches!(jointype, JoinType::JOIN_RIGHT | JoinType::JOIN_FULL | JoinType::JOIN_RIGHT_ANTI), *m);
        }
        let j = gx.pipelines()[q].ops.iter().position(|op| matches!(op, Op::Join(JoinDesc::Probe { slot, .. }) if *slot == br.slot));
        let skip = j.map_or(0, |j| outer(gx, k, q, j).skip);
        defer = j.is_some_and(|j| defers(gx, k, q, j));
        // M7-SPILL B4c / B4c-2 (V4R-414): a keeping or marking build spills too (its drain sends each partition's drain part).
        // D1511: a build's Arms run spills as one build: its bits are its last pipeline's (the shape's build), decided by
        // the arm made first and held on the shape, so every arm opens the slot at one PBITS; the driver finishes the run
        // with no owner fold (D4054). D2717: op 14's deferred build spills too (its held error stamps FAILED first)
        let bits = |px: &Px<'_, '_>, b: usize, at: usize| crate::ops::hashjoin::spill::build::build_bits(px, b, q, at).ok_or_else(|| crate::ops::hashjoin::spill::build::no_bits(px, b, q, at));
        spill = match (run, j) {
            (_, None) => Err(NoSpill::BuildNoProbeOp),
            (true, Some(j)) => sh.run_bits(|| bits(px, sh.build, j)),
            (false, Some(j)) => bits(px, k, j),
        };
        proto = proto.peer(q as u16).arg(skip << 8 | defer as u64);
    } else if let Some((ik, j)) = crate::ops::hashjoin::probe::nest_probe_of(gx, k, br.slot) {
        // D1018 (join#346 / #347): a build only a nest inner probes, of the nest's own set, whose side op 14 holds per
        // re-open (probe.rs `nest_holds`): the merge holds its error for that probe's first row
        defer = crate::ops::hashjoin::probe::nest_holds(gx, ik, j, k) && defers(gx, k, ik, j);
        proto = proto.arg(defer as u64);
        spill = Err(NoSpill::BuildNestInner);
    }
    // B4d: a continuing arm's pipeline merges its workers' partials with the v4 build's merge too (HOSTS' merge_for by
    // pipeline; the runner's fallback is a hand kernel's): its BuildSide names no reading and no skip, only op 14's bit
    let arm = Proto::new(ProtoKind::BuildSide, px.ident(k) as u16).slot(br.slot).arg(defer as u64);
    px.declare_proto(if cont { arm } else { proto });
    // PS1-21 / PS1-26: a deferred build (op 14) declares the rtfilter too: a held error stamps the region FAILED, whose
    // export is no filter (rtfilter raw.rs), so the probe's first row still reaches the probe and raises PG's error
    if !cont { crate::ops::hashjoin::strategies::rtfilter::declare(px, &sh, decls) }
    // B4d: a run's arms share one record layout; the hoisted payload column (hoist.rs) is one pipeline's, so a run has none
    let hoist = if run { None } else { crate::ops::hashjoin::strategies::hoist::build(px, &sh, node) };
    // USEROP: a guarded build is one pipeline's (an arm run's region is opened by its first arm) and never spills
    let guard = crate::ops::shared::one_batch::guard_of(gx, k);
    if guard.is_some() && run {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy(crate::rep::keylane::USER_BATCHES) })
    }
    let spill = if guard.is_some() { Err(NoSpill::BuildGuarded) } else { spill };
    let (spill, why) = match spill {
        Ok(bits) => (bits, None),
        Err(w) => (0, Some(w)),
    };
    let fp128 = matches!(&decls[..], [d] if crate::ops::hashjoin::strategies::rtfilter::text_fp_key(&d.key));
    mcx::pg_box_dyn!(mcx, BuildEnd { sh, facts: gx.facts, keys, node, hoist, compact: Default::default(), range0: Default::default(), keep, marks, spill, why, br, arms: run, foreign, guard, fp128 } => dyn EndMech + 'g)
}

/// D4054: where build end `k` (graph pipeline `k`, slot `slot`) stands among the program pipelines that append its records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Run {
    /// One pipeline builds it.
    Lone,
    /// An arm of an `Arms` run of graph pipelines `first ..= last` (source/append.rs's union arms; a seeded drain's, its
    /// probe's copy made under an earlier identity before the run is declared): D2854's arms. A copy that is not the run's
    /// last arm `continues`.
    Foreign { first: usize, last: usize, continues: bool },
    /// Probe pipeline `k` and its pair pass `P'` (spill/pair.rs `pair`): `P'`'s copy of the end is made first and finishes
    /// the build (`owner`: the reading, the skip mask, the rtfilter); `k`'s own copy, made after, continues it.
    Pair { owner: bool },
}

impl Run {
    /// The graph pipeline whose build shape the end reads: a foreign run's last arm, else `k`.
    pub(crate) fn last(self, k: usize) -> usize {
        match self {
            Run::Foreign { last, .. } => last,
            _ => k,
        }
    }

    /// Another arm finishes the build: this one appends records only.
    pub(crate) fn continues(self) -> bool {
        match self {
            Run::Lone => false,
            Run::Foreign { continues, .. } => continues,
            Run::Pair { owner } => !owner,
        }
    }
}

/// D4669: whether build end `b` (graph pipeline `b`, slot `slot`) is not a lone run (`run_of`: an arm of an `Arms` run, or a
/// pair run), whose rows roads key.dependent / key.carry do not key by (strategies/dep_key.rs). An `Arms` run of another
/// end that reuses the slot number (a union over two groups' emits, declared after the build) is not this build's: the old
/// test (any `Arms` protocol on the slot) declined only once that run was declared, so a group's emit read the codes its
/// fill had kept as datum copies.
pub(crate) fn arms_run(px: &mut Px<'_, '_>, b: usize, slot: u8) -> bool {
    run_of(px, b, slot) != Run::Lone
}

/// D4054: `role` for build end `k` in `px` (`Px::ident`, its `Arms` protocols, the runs a pair grew, the pair runs).
fn run_of(px: &mut Px<'_, '_>, k: usize, slot: u8) -> Run {
    let gx = px.gx;
    let ours = |l: usize| gx.pipelines().get(l).is_some_and(|p| crate::frame::iface::breaker_of(&p.desc).is_some_and(|b| b.slot == slot && b.kind == gravity_abi::breaker::BreakerKind::HashBuild));
    let ident = px.ident(k);
    let m = px.mcx();
    let e = core::mem::replace(px.ext_mut::<crate::ops::hashjoin::build_side::JoinExt>(crate::face::kinds::Face::Join), crate::ops::hashjoin::build_side::JoinExt::new(m));
    let r = role(&px.protos, &e.grown, &e.pair_runs, k, ident, slot, ours);
    *px.ext_mut::<crate::ops::hashjoin::build_side::JoinExt>(crate::face::kinds::Face::Join) = e;
    r
}

/// D4054: build end `k` of `slot`, made under identity `ident`, given the program's protocols, the `Arms` runs a pair
/// grew (`grown`) and the pair runs (`pairs`, spill/pair.rs `pair`); `ours(l)`: graph pipeline `l` ends in this build.
/// - A pair run of `k` on `slot`: `P'`'s copy (`ident` = `P'`) owns it.
/// - Else the `Arms` run on `slot` whose span holds `k` and whose last graph pipeline (`spill/pair.rs run_last`: less each
///   `P'` it grew by) ends in this build: a run that a pair grew keeps its arms. A run whose last pipeline is not a build
///   of `slot` is another end's that reuses the number (a Sort over an Append of partitionwise joins: each arm's builds
///   take the Sort's free number between its arms); `k` is not its arm.
/// - A copy made under an earlier identity continues (a seeded drain's probe copy, made before its run is declared).
fn role(protos: &[Proto], grown: &[usize], pairs: &[(usize, usize, u8)], k: usize, ident: usize, slot: u8, ours: impl Fn(usize) -> bool) -> Run {
    if let Some(&(_, v, _)) = pairs.iter().find(|&&(q, _, s)| q == k && s == slot) {
        return Run::Pair { owner: ident == v };
    }
    let runs = protos.iter().enumerate().filter(|(_, a)| a.kind == ProtoKind::Arms && a.slot == slot);
    let spans = runs.filter_map(|(i, a)| crate::ops::hashjoin::spill::pair::run_last(grown, i, a).map(|l| (a.pipeline as usize, l)));
    let (first, last) = spans.filter(|&(f, l)| (f..=l).contains(&k) && ours(l)).max_by_key(|r| r.1).unwrap_or((k, k));
    let continues = last != k || ident < k;
    if first < last || continues {
        Run::Foreign { first, last, continues }
    } else {
        Run::Lone
    }
}

/// join.buildside's swapped build (build_side.rs `Side`): program pipeline `s.build` runs graph pipeline `s.probe`'s source
/// and its ops before the probe, so the records are the probe side's rows under the probe op's keys (no rtfilter: its
/// filter would sit on the build's own scan).
pub(crate) fn make_swapped<'g>(px: &mut Px<'g, '_>, s: &Swap) -> PgBox<'g, dyn EndMech + 'g> {
    let p = &px.gx.pipelines()[s.probe];
    let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= s.at).map_or(p.node_id, |n| n.node_id);
    let Op::Join(JoinDesc::Probe { keys, decls, .. }) = &p.ops[s.at] else { crate::refuse!(node, Reason::Bug { payload: "a swap without its probe" }) };
    check_keys(px.gx.facts, node, decls, keys.len());
    let mcx = px.mcx();
    let keys = keys.iter().zip(decls.iter()).map(|(n, d)| key_src(px, n, node, d)).collect_in(mcx);
    let sh = crate::ops::hashjoin::build_side::shape_swapped(px, s);
    px.declare_proto(Proto::new(ProtoKind::BuildSide, s.build as u16).slot(sh.slot).peer(s.probe as u16).reading(0, rows));
    // PS1-21 / PS1-26 step 1: the swapped build's keys filter the scan the swap made the probe
    crate::ops::hashjoin::strategies::rtfilter::declare_swapped(px, s, &sh, decls);
    let fp128 = matches!(&decls[..], [d] if crate::ops::hashjoin::strategies::rtfilter::text_fp_key(&d.key));
    // USEROP: a sort over a swapped probe meets the probe side's rows in the build's order, never PostgreSQL's
    if crate::ops::shared::one_batch::guard_of(px.gx, s.build).is_some() {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy(crate::rep::keylane::USER_BATCHES) })
    }
    let br = crate::frame::iface::breaker_of(&px.gx.pipelines()[s.build].desc).unwrap_or(Breaker::sized(BreakerKind::HashBuild, 0));
    mcx::pg_box_dyn!(px.mcx(), BuildEnd { sh, facts: px.gx.facts, keys, node, hoist: None, compact: Default::default(), range0: Default::default(), keep: false, marks: false, spill: 0, why: Some(NoSpill::BuildSwapped), br, arms: false, foreign: false, guard: None, fp128 } => dyn EndMech + 'g)
}



fn st8(b: &mut FunctionBuilder<'_>, r: Value, off: usize, v: u8) {
    let c = b.iconst(Type::I8, v as i64);
    b.store(r, off as i32, c);
}

/// A payload image's bytes in the build's region, from its end down; past the records it leaves by `l_over`.
struct Space {
    r: Value,
    floor: Value,
    l_over: Block,
}

impl KeepSpace for Space {
    fn alloc(&mut self, b: &mut FunctionBuilder<'_>, _home: RetainHome, bytes: Value) -> Value {
        let top = b.load(Type::I64, self.r, TOP as i32);
        let (seven, mask) = (b.iconst(Type::I64, 7), b.iconst(Type::I64, !7));
        let up = b.int_bin(IntBinOp::Add, bytes, seven);
        let al = b.int_bin(IntBinOp::And, up, mask);
        let nt = b.int_bin(IntBinOp::Sub, top, al);
        let over = b.icmp(IntCond::Slt, nt, self.floor);
        let l_ok = b.create_block(&[]);
        b.cond_br(over, (self.l_over, &[]), (l_ok, &[]));
        b.switch_to(l_ok);
        b.store(self.r, TOP as i32, nt);
        b.ptr_add(self.r, nt, 1, 0)
    }
}

/// The word a record keeps for a kept cell: a lane reached untouched is read (a Word), any other `val` is the word itself.
fn stored_word(lx: &mut Lx<'_, '_, '_>, node: i32, k: &rep::Kept, tf: &gravity_decompose::catalog::typefacts::TypeFacts) -> (Value, Value) {
    let null = match k.null {
        Some(n) => n,
        None => lx.b.bconst(false),
    };
    match (k.cell.at, k.cell.rep) {
        (At::Lane(_), Rep::Word { .. }) => rep::datum(lx, CellRef { cell: &k.cell, val: k.val, null: k.null }, tf),
        (At::Lane(_), _) => crate::refuse!(node, Reason::Bug { payload: "a kept payload cell still at its lane" }),
        _ => (k.val, null),
    }
}

/// D254: region `r` (records `n` of `rec` bytes, this row's images undone back to `top0`) reopened through the driver's
/// `region_grow`, the images kept at its new end; the new base (`I64`). A real allocation failure raises the host's error.
fn grow(b: &mut FunctionBuilder<'_>, env: &Env, r: Value, n: Value, rec: usize, base: usize, top0: Value) -> Value {
    b.store(r, TOP as i32, top0);
    let end = b.load(Type::I64, r, END as i32);
    let tail = b.int_bin(IntBinOp::Sub, end, top0);
    let recc = b.iconst(Type::I64, rec as i64);
    let recs = b.int_bin(IntBinOp::Mul, n, recc);
    let hdr = b.iconst(Type::I64, base as i64);
    let head = b.int_bin(IntBinOp::Add, recs, hdr);
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    let f = gravity_kernels::host_abi::region_grow as *const () as usize;
    let h = b.declare_host_once("region_grow", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::I64), f, flags);
    let nb = b.call_host(h, &[env.ctx, recc, head, tail], Some(Type::I64)).expect("region_grow returns the new base");
    let failed = b.icmp(IntCond::Eq, nb, env.zero);
    let (e, l_ok) = (b.iconst(Type::I64, raise::bit(raise::HOST) as i64), b.create_block(&[]));
    b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
    b.switch_to(l_ok);
    let rn = b.conv(ConvOp::IntToPtr, Type::Ptr, nb);
    let limit = b.load_zext(Type::I32, rn, LIMIT as i32);
    let top = b.int_bin(IntBinOp::Sub, limit, tail);
    b.store(rn, END as i32, limit);
    b.store(rn, TOP as i32, top);
    nb
}

impl BuildEnd<'_> {
    /// join.dedup on this build: its probe's end keeps one copy of a row (strategies/dedup.rs), and the probe neither marks
    /// nor fills the inner.
    fn dedup(&self) -> bool {
        self.sh.dedup.get() == crate::ops::hashjoin::strategies::dedup::ON && !self.marks && !self.keep
    }

    /// D4904: the probe reads this build for its keys alone (dedup.rs `keys`), and the build may spill in the full record
    /// form (its rows that find no room go through `spill_grow`, where the records are compacted).
    fn keys(&self) -> bool {
        self.sh.dedup.get() == crate::ops::hashjoin::strategies::dedup::KEYS && !self.marks && !self.keep && self.spill != 0 && !self.compact.get()
    }

    /// The forms the probe reads. A marking probe's match marks its record's address (`mark_at`), so its build lays only
    /// the forms that keep every record in place: KeyHash, and DCHAIN of the dense forms (dense raw.rs `lay`: no DENSE
    /// slots under `A_MARKS`; AGA-22).
    fn allow(&self) -> u8 {
        let a = self.sh.allow | if self.sh.mult.get() { A_MULT } else { 0 };
        match self.marks {
            true => a | A_MARKS | if self.keep { A_KEEP } else { 0 },
            false => a,
        }
    }

    /// The row's image words; a kept NULL-key row's are 0 and its key is never imaged (L0-d): the imaging branch's memo is
    /// dropped, its values reach no later block.
    fn image_words(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cells: &crate::ops::hashjoin::keys::KeyVals<'_>, null: Option<Value>) -> Vec<Value> {
        let l = null.map(|nl| {
            let (l_img, l_null) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(nl, (l_null, &[]), (l_img, &[]));
            b.switch_to(l_img);
            l_null
        });
        let mut lx = env.lx(b, self.node);
        let im = image(&mut lx, self.node, cells, &self.keys[..]);
        self.range0.set(im.words.words.len() > 1 && im.widths.first() == Some(&1));
        let img = im.words.words;
        env.memo = lx.memo;
        let Some(l_null) = l else { return img };
        env.memo.clear();
        let tys: PgVec<'_, Type> = img.iter().map(|&w| b.value_type(w)).collect_in(env.mcx);
        let l_rec = b.create_block(&tys[..]);
        b.br(l_rec, &img);
        b.switch_to(l_null);
        let zeros: PgVec<'_, Value> = tys.iter().map(|&t| b.iconst(t, 0)).collect_in(env.mcx);
        b.br(l_rec, &zeros[..]);
        b.switch_to(l_rec);
        b.block_params(l_rec).to_vec() // alloc: Program
    }
}

impl EndMech for BuildEnd<'_> {
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        let r = region(b, env, None);
        let phase = b.load_zext(Type::I8, r, PHASE as i32);
        let fresh = b.icmp(IntCond::Eq, phase, env.zero);
        let (l_init, l_on) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(fresh, (l_init, &[]), (l_on, &[]));
        b.switch_to(l_init);
        let limit = b.load_zext(Type::I32, r, LIMIT as i32);
        b.store(r, TOP as i32, limit);
        b.store(r, END as i32, limit);
        b.store(r, N as i32, env.zero);
        let (mx, mn) = (b.iconst(Type::I64, i64::MAX), b.iconst(Type::I64, i64::MIN));
        b.store(r, LO as i32, mx);
        b.store(r, HI as i32, mn);
        st8(b, r, PHASE, 1);
        st8(b, r, ALLOW, self.allow());
        st8(b, r, 15, KIND);
        if let Some((tup, first)) = self.guard {
            let t = b.iconst(Type::I16, tup as i64);
            b.store(r, GUARD as i32, t);
            st8(b, r, GUARD_BUCKETS, first);
        }
        crate::ops::hashjoin::spill::build::init(b, r, self.spill);
        if self.keep {
            b.store(r, KEPT as i32, env.zero);
        }
        b.br(l_on, &[]);
        b.switch_to(l_on);
    }

    fn carried<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> PgVec<'e, Value> {
        let r = region(b, env, None);
        let mut v: PgVec<'e, Value> = [N, LO, HI].iter().map(|&o| b.load(Type::I64, r, o as i32)).collect_in(env.mcx);
        v.push_in(b.conv(ConvOp::PtrToInt, Type::I64, r));
        if self.keep {
            v.push_in(b.load(Type::I64, r, KEPT as i32));
        }
        v
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        let (n, lo, hi, ri) = (row.carry.accs[0], row.carry.accs[1], row.carry.accs[2], row.carry.accs[3]);
        let types: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let l_join = b.create_block(&types[..]);
        // D1898: a build that keeps no NULL key (PG's `keep_nulls` = HJ_FILL_INNER) stops at its first NULL key
        let carry = row.carry.params();
        // EXPLAIN ANALYZE: PG's Hash counts the rows it inserts, so a skipped NULL key's row is taken back on its way out
        let skip = (!self.keep && crate::frame::instr::bounded()).then(|| b.create_block(&[]));
        let stop = skip.map_or((l_join, &carry[..]), |l| (l, &[][..]));
        let cells = key_cells(b, env, self.node, row, &self.keys[..], (!self.keep).then_some(stop));
        let null = any_null(b, &cells);
        if let (Some(nl), false) = (null, self.keep) {
            let l_go = b.create_block(&[]);
            b.cond_br(nl, stop, (l_go, &[]));
            b.switch_to(l_go);
        }
        if let Some(l_skip) = skip {
            let here = b.current();
            b.switch_to(l_skip);
            crate::frame::instr::uncount_end_row(b, env.ctx);
            b.br(l_join, &carry);
            b.switch_to(here);
        }
        let verdict = self.hoist.map(|(_, ex, _)| crate::step::expr::emit(b, env, self.node, row, ex));
        let null = null.filter(|_| self.keep);
        let img = self.image_words(b, env, &cells, null);
        let words = img.len();
        // join.dense's bits form: a key of at most 32 sign-extended bits and at most one one-byte payload write compact 8 B
        // records (dense_bits.rs)
        let tf = |t: u32| gravity_decompose::catalog::typefacts::facts_of(self.facts, t);
        let pay: PgVec<'_, _> = self.sh.payload.iter().map(|&a| if self.hoist.is_some_and(|h| h.0 == a) { tf(16) } else { row.col(a).and_then(|c| tf(c.cell.typid)) }).collect_in(env.mcx);
        let one = !self.marks && words == 1 && cells.len() == 1 && matches!(cells[0].0.rep, Rep::Word { .. });
        let vbits = one.then(|| crate::ops::hashjoin::strategies::dense_bits::eligible(&self.sh, &cells[0].3, &pay[..])).flatten();
        (self.compact.set(vbits.is_some()), self.sh.vbits.set(vbits.unwrap_or(1)));
        // join.payload_fp (strategies/payload_fp.rs): the code payload columns a group past the probe keys by carry their
        // Fp128 words after the payload, in a plain record of a lone build (no compact record, Arms or pair run, hoist); a
        // plain text value (a merged scan's datum lane, a heap column) carries its bytes' Fp128 where the statement images
        // its text keys so (`plain_image`)
        let plain = crate::ops::hashjoin::strategies::payload_fp::plain_image(env.core.strategies);
        let fp = |a: i16, c: &CellRef<'_>| {
            let bp = self.sh.fp.cols().iter().find(|f| f.0 == a).map(|f| f.1);
            // a lane's code reads its table's entry, a tableless one its render (a record code's table is past its window);
            // a text or varchar value hashes its bytes
            let code = match c.cell.rep {
                Rep::Code { dict } => !c.cell.image_only() && (matches!(c.cell.at, At::Lane(_)) || dict.fp_table == gravity_program::DictRef::NO_TABLE),
                Rep::VarlenaPtr => plain && !c.cell.image_only() && gravity_fn::typesem::text_payload(c.cell.typid),
                _ => false,
            };
            bp.is_some_and(|bp| code && !self.hoist.is_some_and(|h| h.0 == a) && (crate::dict::coderep::sk_bpchar_of(tf(c.cell.typid).as_ref()) != 0) == bp)
        };
        let live = match vbits.is_some() || self.arms || self.foreign {
            true => 0,
            false => self.sh.payload.iter().take(64).enumerate().filter(|&(_, &a)| row.col(a).is_some_and(|c| fp(a, &c))).fold(0u64, |m, (i, _)| m | 1 << i),
        };
        if live != 0 {
            fires::fire("join.payload_fp"); // shapecheck fire signal (join.payload_fp): the build stores its text payloads' Fp128 words
        }
        let full = 8 * (words + 1 + self.sh.payload.len() + crate::ops::hashjoin::layout::null_words(self.sh.payload.len()) + 2 * live.count_ones() as usize);
        self.sh.fp_live.set(live);
        (self.sh.words.set(words as u8), self.sh.rec.set(full as u32), self.sh.range.set(words == 1));
        if self.arms {
            self.sh.arm_form(self.node, (words as u8, full as u32, self.compact.get(), self.sh.vbits.get()));
        }
        let rec = if self.compact.get() { 8 } else { full };
        // D254: the record and its images go in at the region `ri`; past its room the region grows (at least doubling) and
        // the row retries. D1029: the grow path checks cancel (the row path takes none); a region at the driver's MAX_REGION
        // does not grow (region_grow's out-of-memory, the build's error), so no retry repeats on a region that freed no room.
        // M7-SPILL: a build that may spill grows within its limit, else writes partitions out (at most twice) and retries
        // with the records kept (`n`)
        let spill = self.spill != 0 && !self.compact.get();
        let l_try = b.create_block(if spill { &[Type::I64, Type::I64] } else { &[Type::I64] });
        let args = if spill { mcx::pg_vec![in env.mcx; ri, n] } else { mcx::pg_vec![in env.mcx; ri] };
        b.br(l_try, &args[..]);
        b.switch_to(l_try);
        b.mark_loop_header(l_try);
        let ri = b.block_params(l_try)[0];
        let n = if spill { b.block_params(l_try)[1] } else { n };
        let r = b.conv(ConvOp::IntToPtr, Type::Ptr, ri);
        let top0 = b.load(Type::I64, r, TOP as i32);
        let recc = b.iconst(Type::I64, rec as i64);
        let off = b.int_bin(IntBinOp::Mul, n, recc);
        let p = b.ptr_add(r, off, 1, self.sh.base() as i32);
        let endc = b.iconst(Type::I64, (self.sh.base() + rec) as i64);
        let end = b.int_bin(IntBinOp::Add, off, endc);
        let over = b.icmp(IntCond::Sgt, end, top0);
        let (l_grow, l_fit) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(over, (l_grow, &[]), (l_fit, &[]));
        b.switch_to(l_fit);
        for (i, &w) in img.iter().enumerate().filter(|_| !self.compact.get()) {
            b.store(p, 8 * i as i32, w);
        }
        let mut space = Space { r, floor: end, l_over: l_grow };
        let mut lx = env.lx(b, self.node);
        lx.space = Some(&mut space);
        let mut nulls = env.zero;
        let mut wide = mcx::pg_vec![in env.mcx; env.zero; crate::ops::hashjoin::layout::null_words(self.sh.payload.len())];
        let mut kept = Vec::with_capacity(self.sh.payload.len()); // alloc: Program
        let (mut rel, mut quiet, mut compact) = ([0u64; crate::ops::hashjoin::layout::REL_WORDS], [0u64; crate::ops::hashjoin::layout::REL_WORDS], None);
        for (i, &a) in self.sh.payload.iter().enumerate() {
            let own;
            let c = match (self.hoist, verdict) {
                (Some((col, _, out)), Some((v, ok))) if col == a => {
                    own = Cell::of_expr(out, row.row.cells.len() as u16, 0, Nullable::Maybe);
                    let t = lx.b.bconst(true);
                    CellRef { cell: &own, val: v, null: Some(lx.b.int_bin(IntBinOp::Xor, ok, t)) }
                }
                _ => row.col(a).unwrap_or_else(|| crate::refuse!(self.node, Reason::Shape { why: ShapeWhy("a join payload column the build row does not carry") })),
            };
            let tf = crate::step::expr::tf_datum(c.cell.typid);
            // D2854: a reader renders a record's code through one site, the run's last arm's: an arm of a foreign run keeps
            // its image
            let shown;
            let c = match c.cell.rep {
                Rep::Code { .. } if self.foreign => {
                    let (v, none) = rep::datum(&mut lx, c, &tf);
                    shown = Cell { rep: Rep::VarlenaPtr, at: At::Value(row.row.cells.len() as u16), null: Nullable::Maybe, ..*c.cell };
                    CellRef { cell: &shown, val: v, null: Some(none) }
                }
                _ => c,
            };
            // join.payload_fp: the code's Fp128 (its lane's table entry, or the words a record code carries, else its render's),
            // a plain value's its bytes' (or the words a record carries), `[0, 0]` for a NULL, as the table's image of one
            let fpw = self.sh.fp_at(i).map(|j| match rep::hash_image(&mut lx, c, &tf, rep::Scope::Program) {
                rep::Words { w: [Some(lo), Some(hi)], exact: true } => match c.null {
                    Some(n) => {
                        let z = lx.b.iconst(Type::I64, 0);
                        (j, lx.b.select(n, z, lo), lx.b.select(n, z, hi))
                    }
                    None => (j, lo, hi),
                },
                _ => crate::refuse!(self.node, Reason::Bug { payload: "a payload code whose image is not its two Fp128 words" }),
            });
            // D333: by pointer only bytes that outlive every reader of the build (Statement); a record's or breaker's are copied
            let by_ref = matches!(c.cell.rep, Rep::VarlenaPtr | Rep::CstrPtr | Rep::FixedPtr { .. });
            let until = if by_ref { Until::Statement } else { Until::Life };
            // D2854 (Hibernate's union-subclass join: `cast(null as varchar)` in two arms of a probe-side UNION ALL, a heap
            // column in the third): a run's arms keep every by-reference column as an image. A literal's or a parameter's
            // bytes outlive the statement, and its NULL flag is a word of the program, not a constant (literals are not
            // baked into a program), so an arm over one would keep it by pointer while an arm over a scan copies
            let copy = self.arms && by_ref;
            let home = RetainHome::Record(LifeId(self.sh.build as u16));
            let k = if copy { rep::retain_copy(&mut lx, c, &tf, until, home) } else { rep::retain(&mut lx, c, &tf, until, home) };
            let (mut w, nul) = stored_word(&mut lx, self.node, &k, &tf);
            if lx.b.const_of(nul).is_some_and(|(_, v)| v != 0) {
                quiet[i / 64] |= 1 << (i % 64);
            }
            if (copy || !c.cell.life.reaches(until)) && matches!(k.cell.rep, Rep::VarlenaPtr | Rep::CstrPtr | Rep::FixedPtr { .. }) {
                if lx.b.value_type(w) == Type::Ptr { w = lx.b.conv(ConvOp::PtrToInt, Type::I64, w) }
                let off = lx.b.int_bin(IntBinOp::Sub, w, ri);
                let end = lx.b.load(Type::I64, r, END as i32);
                w = lx.b.int_bin(IntBinOp::Sub, end, off);
                rel[i / 64] |= 1 << (i % 64);
            }
            // join.payload_fp: a record's reader keys a code by the words it carries (`Env::fps`), never by its lane's table
            // (declared for the build's scan; another payload column of the lane reads its render)
            let mut kc = k.cell;
            if let Rep::Code { dict } = kc.rep {
                // a code a record handed on (another build's, multi-hop, or a group's emit) keeps that record's home, so a
                // reader renders it through the site of the pipeline that made it (walk.rs `joined`; spill/forms.rs's home
                // rule), with join.payload_fp or without: D4445, a build fed by a group's emit has no site of its own, so
                // the build's home left a code with no dictionary to render through
                let made = match (c.cell.life.home, self.arms || self.foreign) {
                    (crate::cell::CellHome::Record(l0), false) => Some(l0),
                    _ => None,
                };
                if !self.sh.fp.cols().is_empty() || made.is_some() {
                    kc.rep = Rep::Code { dict: gravity_program::DictRef { fp_table: gravity_program::DictRef::NO_TABLE, ..dict } };
                }
                if let Some(l0) = made {
                    kc.life = crate::cell::CellLife { until: Until::Statement, home: crate::cell::CellHome::Record(l0) };
                }
            }
            kept.push(kc); // alloc: Program
            if self.compact.get() {
                compact = Some((w, nul));
                continue;
            }
            lx.b.store(p, self.sh.pay_off(i), w);
            if let Some((j, lo, hi)) = fpw {
                lx.b.store(p, self.sh.fp_off(j), lo);
                lx.b.store(p, self.sh.fp_off(j) + 8, hi);
            }
            let z = lx.b.conv(ConvOp::Zext, Type::I64, nul);
            let (off, at) = self.sh.null_at(i);
            let sh = lx.b.iconst(Type::I64, at as i64);
            let bit = lx.b.int_bin(IntBinOp::Shl, z, sh);
            match off == self.sh.link_off() {
                true => nulls = lx.b.int_bin(IntBinOp::Or, nulls, bit),
                false => {
                    let w = &mut wide[(i - crate::ops::hashjoin::layout::LINK_NULLS) / 64];
                    *w = lx.b.int_bin(IntBinOp::Or, *w, bit);
                }
            }
        }
        // D2854: each arm of a build's Arms run (B4d) lowers this end; the shape keeps one form for all of them. An end
        // outside a run that is made again (a MERGE's spooled form, writesubplan_cte_drain) keeps its last making's form
        if self.arms {
            self.sh.settle_arm(self.node, kept, rel, quiet);
        } else {
            *self.sh.kept.borrow_mut() = kept;
            self.sh.rel.set(rel);
        }
        for (j, &w) in wide.iter().enumerate() {
            let (off, _) = self.sh.null_at(crate::ops::hashjoin::layout::LINK_NULLS + 64 * j);
            lx.b.store(p, off, w);
        }
        match self.compact.get() {
            true => crate::ops::hashjoin::strategies::dense_bits::store(lx.b, p, img[0], compact, self.sh.vbits.get()),
            false => {
                // L0-d: a kept NULL-key record's link says so until the lay-out moves it past the keyed ones
                if let Some(nl) = null {
                    let (k, z) = (lx.b.iconst(Type::I64, NULLKEY as i64), lx.b.iconst(Type::I64, 0));
                    let f = lx.b.select(nl, k, z);
                    nulls = lx.b.int_bin(IntBinOp::Or, nulls, f);
                }
                lx.b.store(p, self.sh.link_off(), nulls)
            }
        }
        let add = spill.then(|| crate::ops::hashjoin::spill::build::route_row(lx.b, env, r, n, top0));
        let (mut lo2, mut hi2) = match img[..] {
            [w] => (lx.b.int_bin(IntBinOp::Smin, lo, w), lx.b.int_bin(IntBinOp::Smax, hi, w)),
            [w, ..] if self.range0.get() => (lx.b.int_bin(IntBinOp::Smin, lo, w), lx.b.int_bin(IntBinOp::Smax, hi, w)),
            _ => (lo, hi),
        };
        let n2 = lx.b.int_bin(IntBinOp::Add, n, add.unwrap_or(env.one));
        let mut out = mcx::pg_vec![in env.mcx; row.carry.out, n2, lo2, hi2, ri];
        // L0-d: a keeping build carries its kept count (`carried`) through every row: one more for a NULL-key row, the
        // count unchanged when no key can be NULL (a NOT NULL key column: no `null` word)
        match null {
            Some(nl) => {
                (lo2, hi2) = (lx.b.select(nl, lo, lo2), lx.b.select(nl, hi, hi2));
                let one = lx.b.conv(ConvOp::Zext, Type::I64, nl);
                out = mcx::pg_vec![in env.mcx; row.carry.out, n2, lo2, hi2, ri, lx.b.int_bin(IntBinOp::Add, row.carry.accs[4], one)];
            }
            None if self.keep => out.push_in(row.carry.accs[4]),
            None => {}
        }
        env.memo = lx.memo;
        b.br(l_join, &out[..]);
        b.switch_to(l_grow);
        crate::frame::cancel::check(b, env.ctx, env.l_cancel);
        match spill {
            true => {
                self.stamp(b, r);
                let (rt, kept) = crate::ops::hashjoin::spill::build::grow(b, env, r, n, rec, top0);
                b.br(l_try, &[rt, kept]);
            }
            false => {
                let rt = grow(b, env, r, n, rec, self.sh.base(), top0);
                b.br(l_try, &[rt]);
            }
        }
        b.switch_to(l_join);
        Carry::from_params(b.block_params(l_join), env.mcx)
    }

    fn window_end(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env, c: &Carry) {
        let r = b.conv(ConvOp::IntToPtr, Type::Ptr, c.accs[3]);
        for (o, &v) in [N, LO, HI].iter().zip(c.accs.iter()) {
            b.store(r, *o as i32, v);
        }
        if self.keep {
            b.store(r, KEPT as i32, c.accs[4]);
        }
        self.stamp(b, r);
    }

    /// M7-SPILL: the staged records written at each claim's close, when the build spilled.
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &Exits) {
        if self.spill != 0 && !self.compact.get() {
            let r = region(b, env, None);
            crate::ops::hashjoin::spill::build::flush(b, env, r, 0);
        }
    }

    /// The records' bytes for the admit estimate (D272). The graph's legacy spill-bits word (decompose join.rs `bits_word`)
    /// goes: it names a slot of v4's own word plan (a literal's), and a v4 build grows by name or spills its own
    /// partitions (join/build_spill.rs), never through the driver's scatter.
    fn pipeline_desc(&self, _px: &Px<'_, '_>, _k: usize, d: Pipeline) -> Pipeline {
        Pipeline { store_row_bytes: 8 * (2 * self.keys.len() + 1 + self.sh.payload.len() + 2 * self.sh.fp.cols().len()) as u32, words: None, ..d }
    }

    /// C10: the build spills its own partitions (`spill` bits, the record form not compact), else grows by name for its
    /// named reason; the descriptor's breaker keeps decompose's declared bits (cost reads them).
    fn holds(&self) -> Option<Holds> {
        let shape = match (self.spill, self.why) {
            (0, w) => spill::Shape::Grows(w.unwrap_or(NoSpill::Unrouted)),
            (_, _) if self.compact.get() => spill::Shape::Grows(NoSpill::BuildCompact),
            (bits, _) => spill::Shape::Spills(Spill::Partitioned { bits }),
        };
        Some(spill::plan(&self.br, shape))
    }
}

impl BuildEnd<'_> {
    /// The header the merge and the spill read: the record's shape and the payload images' kinds.
    fn stamp(&self, b: &mut FunctionBuilder<'_>, r: Value) {
        if self.keys() {
            fires::fire("lower.join.semikeys"); // shapecheck fire signal (D4904): a SEMI / ANTI build stamped to keep one record per key
        }
        st8(b, r, WORDS, self.sh.words.get());
        st8(b, r, PAY, self.sh.payload.len() as u8);
        st8(b, r, crate::ops::hashjoin::layout::PAY_HI, (self.sh.payload.len() >> 8) as u8);
        if self.compact.get() && self.sh.vbits.get() != 1 {
            st8(b, r, VBITS, self.sh.vbits.get());
        }
        st8(b, r, FLAGS, if self.sh.range.get() { F_RANGE } else { 0 } | if self.range0.get() { F_RANGE0 } else { 0 } | if self.compact.get() { F_COMPACT } else { 0 } | if self.fp128 { F_FP128 } else { 0 } | if self.dedup() { F_DEDUP } else { 0 } | if self.keys() { F_KEYS } else { 0 });
        let (rec, full) = (b.iconst(Type::I32, if self.compact.get() { 8 } else { self.sh.rec.get() as i64 }), b.iconst(Type::I32, self.sh.rec.get() as i64));
        (b.store(r, REC as i32, rec), b.store(r, FULL as i32, full));
        for (i, c) in self.sh.kept.borrow().iter().enumerate() {
            let d = match c.rep {
                _ if !self.sh.is_rel(i) => 0,
                Rep::VarlenaPtr => D_VARLENA,
                Rep::CstrPtr => D_CSTR,
                Rep::FixedPtr { len } => len,
                _ => 0,
            };
            let v = b.iconst(Type::I16, d as i64);
            b.store(r, crate::ops::hashjoin::layout::desc_at(i) as i32, v);
        }
    }
}

#[cfg(test)]
mod run_tests {
    use super::{role, Run};
    use gravity_program::protos::{Proto, ProtoKind};

    /// The builds of slot 2 end graph pipelines 2, 3 and 4 (a union's arms, a seeded drain); pipeline 5 ends in another.
    fn ours(l: usize) -> bool {
        (2..=4).contains(&l)
    }

    fn arms(first: usize, n: u64, slot: u8) -> Proto {
        Proto::new(ProtoKind::Arms, first as u16).slot(slot).arg(n)
    }

    /// One pipeline builds it: no run holds it, no pair, its own identity.
    #[test]
    fn a_build_no_run_holds_is_lone() {
        assert_eq!(role(&[], &[], &[], 3, 3, 2, ours), Run::Lone);
        // a run on another slot, and a pair run of another pipeline or slot, are not its
        assert_eq!(role(&[arms(2, 3, 7)], &[], &[(4, 9, 2), (3, 9, 7)], 3, 3, 2, ours), Run::Lone);
        // a run whose last pipeline ends in another build (a Sort's run reusing the number) does not hold it
        assert_eq!(role(&[arms(3, 3, 2)], &[], &[], 3, 3, 2, ours), Run::Lone);
    }

    /// A union's arms 2 ..= 4 into one build: the last arm owns it, the others continue; a seeded drain's probe copy (made
    /// under an earlier identity before its run is declared) continues, and the drain's own copy then finishes the run.
    #[test]
    fn a_foreign_runs_last_arm_finishes_it() {
        let union = [arms(2, 3, 2)];
        assert_eq!(role(&union, &[], &[], 4, 4, 2, ours), Run::Foreign { first: 2, last: 4, continues: false });
        assert_eq!(role(&union, &[], &[], 3, 3, 2, ours), Run::Foreign { first: 2, last: 4, continues: true });
        assert_eq!(role(&union, &[], &[], 2, 2, 2, ours), Run::Foreign { first: 2, last: 4, continues: true });
        assert_eq!(role(&[], &[], &[], 3, 2, 2, ours), Run::Foreign { first: 3, last: 3, continues: true });
        assert_eq!(role(&[arms(2, 2, 2)], &[], &[], 3, 3, 2, ours), Run::Foreign { first: 2, last: 3, continues: false });
        assert!(!Run::Foreign { first: 2, last: 4, continues: false }.continues());
        assert_eq!(Run::Foreign { first: 2, last: 4, continues: true }.last(3), 4);
    }

    /// R6: a union's run 2 ..= 4 that a pair grew by one pass `P'` (its span 4 program pipelines, `grown` naming it once)
    /// keeps its arms: its last graph pipeline is 4, not 5 (another build's), so no arm reads as a lone build.
    #[test]
    fn a_grown_foreign_run_keeps_its_arms() {
        let grown_union = [arms(2, 4, 2)];
        assert_eq!(role(&grown_union, &[0], &[], 4, 4, 2, ours), Run::Foreign { first: 2, last: 4, continues: false });
        assert_eq!(role(&grown_union, &[0], &[], 2, 2, 2, ours), Run::Foreign { first: 2, last: 4, continues: true });
        // read without the growth, the span would end at pipeline 5, another build's: every arm would be lone
        assert_eq!(role(&grown_union, &[], &[], 4, 4, 2, ours), Run::Lone);
        // grown twice (a pair at two of its arms)
        assert_eq!(role(&[arms(2, 5, 2)], &[0, 0], &[], 3, 3, 2, ours), Run::Foreign { first: 2, last: 4, continues: true });
    }

    /// A pair run of probe pipeline 3 (pass `P'` = 9) on slot 2: `P'`'s copy of the end owns the build, pipeline 3's own
    /// copy continues it, whatever `Arms` run the pair declared over them (`arms(3, 2)`, grown once).
    #[test]
    fn a_pair_runs_pass_owns_the_build() {
        let pair = [(3, 9, 2)];
        assert_eq!(role(&[], &[], &pair, 3, 9, 2, ours), Run::Pair { owner: true });
        assert_eq!(role(&[arms(3, 2, 2)], &[0], &pair, 3, 3, 2, ours), Run::Pair { owner: false });
        assert!(!Run::Pair { owner: true }.continues());
        assert!(Run::Pair { owner: false }.continues());
        assert_eq!(Run::Pair { owner: true }.last(3), 3);
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;

    /// D276's reading: a laid-out build gives its N and form, a zeroed header (the runner's unclaimed open after the
    /// finish) an empty build, a build still taking rows nothing.
    #[test]
    fn the_reading_takes_a_zeroed_header_as_an_empty_build() {
        use crate::ops::hashjoin::rt::wr;
        let mut h = [0u64; HDR / 8];
        let d = h.as_mut_ptr() as *mut u8;
        let read = |d: *mut u8| {
            let mut got = Vec::new();
            // SAFETY: `d` is the HDR-byte header above.
            let n = unsafe { rows(d, &[], None, &mut |a, b, c| got.push((a, b, c))) };
            (n, got)
        };
        assert_eq!(read(d), (Some(1), vec![(0, 0, EMPTY as u64)]));
        // SAFETY: offsets inside the header.
        unsafe { (wr::<u8>(d, 15, KIND), wr::<u8>(d, PHASE, 1), wr::<u64>(d, N, 3)) };
        assert_eq!(read(d), (None, vec![]));
        unsafe { (wr::<u8>(d, PHASE, 2), wr::<u8>(d, FORM, DENSE)) };
        assert_eq!(read(d), (Some(1), vec![(0, 3, DENSE as u64)]));
        unsafe { (wr::<u8>(d, 15, 0), wr::<u8>(d, PHASE, 0)) };
        assert_eq!(read(d), (None, vec![]), "a zeroed kind with records is no build of ours");
    }
}
