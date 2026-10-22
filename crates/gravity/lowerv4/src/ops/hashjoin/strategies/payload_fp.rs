//! join.payload_fp: a text group key or DISTINCT argument whose column a hash build's pipeline scans keys by the value's
//! Fp128, taken once per build row, instead of a render and a SipHash per probe row (the projection after a probe renders
//! an INNER_VAR column, and the group hashes the render's bytes for every row that reaches it).
//! - The build's scan declares the lane's fingerprint table (ops/scan/pgrc/lower.rs `lanes`; a bpchar lane's is the
//!   trimmed renders'); the build stores `FpTable[code]` as two words after the record's payload (build.rs, `fp_off`).
//! - The probe links the words to the code word (walk.rs `joined`, `Env::fps`), the projection after it passes the record
//!   cell on as it is (step/project.rs, `forwards`), and rep.code.hash's image and eq read the words before any render. A
//!   draining join's drain links the words of each record it sends the same way (drain/source.rs `row`, join.drain.fp):
//!   its probe's rows and its records run the drain pipeline's ops into one end (drain/lower.rs's tail), so the trace for
//!   such a build follows the drain pipeline (`drain_of`), and both paths see one cell form. A drain that ends in a further
//!   build keeps the render: that build is the seeded drain's Arms run, which keeps plain records.
//!   The entry is the render's Fp128 (rule A), so groups and answers are the same; the group's copy renders once per fresh
//!   record through the build's dictionary (emit_key.rs `keep_datums_in`, D1348).
//! - A plain text value of the build's scan, which no code lane holds (a merged pgrc scan's datum lane: a one-part table
//!   read by its `sorted_by` key keeps its merge mark on a build, D4585; a heap column), stores its bytes' Fp128 instead
//!   (`plain`; rep.varlena.hash's image, taken once a build row), and rep.varlena.hash's image reads the linked words as
//!   rep.code.hash's does: the same words a code of those bytes keys by. It saves the group a SipHash a row, no render, so
//!   it has its own cost rule (cost::payload_plain), and it is taken only where the statement images its text keys as
//!   that Fp128 (`plain_image`).
//! A column outside these gates (`cols`) keeps today's path:
//! - an `Fp128` key or DISTINCT argument of a hashed, DISTINCT, key-ordered hashed group or Unique the probe pipeline ends in,
//!   under a collation whose equal values are equal bytes (D823), fed by the whole pipeline (no streamed group);
//! - a bare column of the row, walked back to the probe (or drain) right before a projection, a payload column of `b`'s records,
//!   which `b`'s row takes from a code lane of its scan, a plain text value of its scan or (multi-hop) from a payload column
//!   another build carries (`sourced`); the probe pipeline ends in the group or in a build that carries the column on (`carried`), whose
//!   readers render the code through the first build's site (build.rs keeps its home);
//! - a bpchar key only over a bpchar column of declared length (D767 / D1353), a non-bpchar key over a non-bpchar one;
//! - the pair cannot swap (join.buildside: only an INNER pair ever swaps), and the cost rule on plan estimates (lower inputs/key.rs `decide`, a guard of
//!   the program): the rows expected at the group times what a row there saves (a render and a SipHash against two loads)
//!   are at least the build's rows times a row's store, both from the class table's measured rows (`pays`).
//! The build keeps out a compact record, an Arms or foreign run and a hoisted column (build.rs `live`); at most `FP_COLS`
//! columns per build, the sets in place (no heap: alloclint). Budget 360 lines. Owner joinfp.
//! Roads (genreg P6: every default-ON decision has an off switch): `SET pgrust.gravity_road_off = 'join.payload_fp'` keeps
//! the render on every build (no record carries the words); `'join.drain.fp'` keeps it on a build whose probe drains (a
//! RIGHT / FULL / RIGHT SEMI / RIGHT ANTI join), so the probe's rows and the drained records reach the end in one cell form.
use gravity_abi::breaker::{BreakerKind, KeyKind};
use gravity_decompose::graph::Source;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::agg::GroupForm;
use gravity_decompose::Op;
use gravity_program::{LaneId, LifeId, Rep};
use types_nodes::jointype::JoinType;
use types_nodes::primnodes::{INNER_VAR, OUTER_VAR};

use crate::graph::Gx;
use crate::ops::hashjoin::face::JoinRoad;
use crate::ops::Table;

crate::strategies! {
    /// A hash build's records carry a text payload column's Fp128 words for the group (or DISTINCT) past its probe.
    JOIN_PAYLOAD_FP = 55, "join.payload_fp", JoinFoldVarlena;
    /// A draining join's drain links each record's carried Fp128 words to its code, as the probe does.
    JOIN_DRAIN_FP = 40, "join.drain.fp", Drain;
}

pub(crate) const ROWS: Table = Table { strategies: &[(JOIN_PAYLOAD_FP, &()), (JOIN_DRAIN_FP, &())], ..crate::ops::NONE };

/// The most columns whose words one build's records carry.
pub(crate) const FP_COLS: usize = 8;

/// Columns (or lanes) with whether each key is bpchar, at most `FP_COLS`, each once, in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FpCols { n: u8, at: [(i16, bool); FP_COLS] }

impl FpCols {
    pub(crate) const NONE: FpCols = FpCols { n: 0, at: [(0, false); FP_COLS] };

    /// `c` joins the set unless its column is in it or the set is full.
    fn add(&mut self, c: (i16, bool)) {
        if (self.n as usize) < FP_COLS && !self.has(c.0) {
            (self.at[self.n as usize], self.n) = (c, self.n + 1);
        }
    }
    pub(crate) fn cols(&self) -> &[(i16, bool)] {
        &self.at[..self.n as usize]
    }
    pub(crate) fn has(&self, a: i16) -> bool {
        self.cols().iter().any(|c| c.0 == a)
    }
}

/// Where a column of a pipeline's row comes from, walked back through the pipeline's ops (`walk`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum From {
    /// The scan's attribute.
    Scan(i16),
    /// Payload column `attno` (an INNER_VAR attno) of the build whose records op `j` hands on (a probe or a drain).
    Inner { j: usize, attno: i16 },
}

/// Column `a` of the row entering op `pos` of pipeline `k`: a filter keeps the row; a projection's column is its entry's,
/// a bare Var (relabels stripped); an outer column past a hash probe is its outer row's (walk.rs `joined` keeps the outer
/// cells); an inner column of a projection right after a probe (or a drain) is that op's payload. `None` for a computed
/// entry, an upper level's Var and any other op (a drain's outer columns are NULL).
pub(crate) fn walk(gx: &Gx<'_, '_>, k: usize, mut pos: usize, mut a: i16) -> Option<From> {
    let ops = &gx.pipelines()[k].ops;
    loop {
        let Some(i) = ops[..pos].iter().rposition(|op| !matches!(op, Op::Filter(_))) else { return Some(From::Scan(a)) };
        match &ops[i] {
            Op::Project(l) => {
                let te = l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == a)?;
                let v = crate::table::emit_group::bare(te.expr).as_var().filter(|v| v.varlevelsup == 0 && v.varattno > 0)?;
                match v.varno {
                    INNER_VAR => {
                        let j = ops[..i].iter().rposition(|op| !matches!(op, Op::Filter(_)))?;
                        return matches!(ops[j], Op::Join(JoinDesc::Probe { .. } | JoinDesc::Drain { .. })).then_some(From::Inner { j, attno: v.varattno });
                    }
                    OUTER_VAR => (pos, a) = (i, v.varattno),
                    r => return gx.pipelines()[k].source.relation().filter(|s| r == s.0 as i32).map(|_| From::Scan(v.varattno)),
                }
            }
            Op::Join(JoinDesc::Probe { .. }) => pos = i,
            _ => return None,
        }
    }
}

/// The row entering pipeline `k`'s end: a hash build's last op is its build op.
pub(crate) fn end_pos(gx: &Gx<'_, '_>, k: usize) -> usize {
    let ops = &gx.pipelines()[k].ops;
    ops.len() - matches!(ops.last(), Some(Op::Join(JoinDesc::Build { .. }))) as usize
}

/// The probe of build `b` (its breaker's `slot`): the later pipeline `q` and op `j` that read `b`'s records (build_side.rs
/// `compute`'s rule).
fn probe_of(gx: &Gx<'_, '_>, b: usize, slot: u8) -> Option<(usize, usize)> {
    gx.pipelines().iter().enumerate().skip(b + 1).find_map(|(q, p)| {
        let j = p.ops.iter().position(|op| matches!(op, Op::Join(JoinDesc::Probe { slot: s, .. }) if *s == slot))?;
        (gx.life_of(q, slot) == Some(LifeId(b as u16))).then_some((q, j))
    })
}

/// join.buildside may swap the pair (build_side.rs `swaps`, asked only where it files no relation guard: a heap probe side
/// decides on its pages, so such a pair is kept out). Only an INNER pair of a plan's own join swaps (build_side.rs `Side`):
/// any other keeps its build whatever its sides.
fn may_swap(gx: &Gx<'_, '_>, b: usize, q: usize, j: usize) -> bool {
    let inner = matches!(gx.pipelines()[q].ops[j], Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_INNER, catalog: false, .. }));
    let (Source::Range { relid: rb, .. }, Source::Range { relid: rp, .. }) = (&gx.pipelines()[b].source, &gx.pipelines()[q].source) else { return false };
    inner && gx.facts.rel_rows(*rb).is_some() && (gx.facts.rel_rows(*rp).is_none() || crate::ops::hashjoin::build_side::swaps(gx, b, q, j))
}

/// The columns of pipeline `q`'s end row that key by a text value's Fp128 (each with whether its key is bpchar), and the
/// rows the planner expects to reach that end (`end_rows`, 0 for none). A Unique keys its records by the hash aggregate's
/// end too (runs.unique); a run op (a streamed group, DISTINCT ON's first of a run) keeps the render.
fn wants(gx: &Gx<'_, '_>, q: usize) -> (FpCols, u64) {
    let p = &gx.pipelines()[q];
    let Some(g) = gx.group_at(q).filter(|g| matches!(g.form, GroupForm::Hashed | GroupForm::Sorted | GroupForm::Distinct | GroupForm::Unique)) else { return (FpCols::NONE, 0) };
    if p.ops.iter().any(|op| matches!(op, Op::GroupRun(_) | Op::FirstOfRun(_))) {
        return (FpCols::NONE, 0);
    }
    let keys = g.keys.iter().filter(|k| k.kind == KeyKind::Fp128 && gx.facts.coll(k.coll).bytes_eq);
    let mut out = FpCols::NONE;
    for c in keys.filter_map(|k| crate::table::emit_group::key_col(gx, q, k).map(|c| (c, crate::dict::coderep::sk_bpchar(gx, k.tf) != 0))) {
        out.add(c);
    }
    (out, p.desc.end_rows)
}

/// join.payload_fp's cost rule (lower inputs/key.rs `Decide`): the rows reaching the group (`a[0]`) times 1000 against the
/// build's rows times a row's store in permille of a group row's saving (`a[1]`, cost::forms::payload_fp_permille); an
/// absent estimate (0) takes the words.
fn pays(build_rows: u64, a: &[u64; 4]) -> u64 {
    (a[0] == 0 || build_rows == 0 || a[0].saturating_mul(1000) >= build_rows.saturating_mul(a[1])) as u64
}

/// The store's permille of the saving (`pays`' `a[1]`), the class table's for the server's widest width; 1000 (the rows at
/// the group at least the build's, the rule of before) for a table without the rows. A build that carries a plain text
/// value's words (`plain`) hashes its bytes where a code reads its table's entry, and saves the group a SipHash, no
/// render: its own break-even (cost::payload_plain), the dearer of the two when it carries both. Its dearer end, not the
/// session's width: the program tier serves this lowering to sessions of every width (`Facts::width`), and the
/// serial-class key holds only for a rule that changes the graph.
fn permille(gx: &Gx<'_, '_>, plain: bool) -> u64 {
    let w = gx.facts.width().unwrap_or(0);
    let code = gravity_program::cost::forms::payload_fp_permille(w).unwrap_or(1000);
    if plain { code.max(gravity_program::cost::payload_plain::permille(w)) } else { code }
}

/// Build `b`'s payload columns (INNER_VAR attnos) whose records carry the Fp128 words, each with whether its key is
/// bpchar (`carried`).
pub(crate) fn cols(gx: &Gx<'_, '_>, b: usize) -> FpCols {
    carried(gx, b).0
}

/// Build `b`'s probe: the later pipeline `q` and op `j` that read its records, when the pair cannot swap.
fn pair(gx: &Gx<'_, '_>, b: usize) -> Option<(usize, usize)> {
    let br = crate::frame::iface::breaker_of(&gx.pipelines()[b].desc).filter(|br| br.kind == BreakerKind::HashBuild)?;
    let (q, j) = probe_of(gx, b, br.slot)?;
    (!may_swap(gx, b, q, j)).then_some((q, j))
}

/// Whether build `b`'s records keep payload column `attno` for probe op `j` of pipeline `q` (build_side.rs `compute`): the
/// columns the probe reads after it (`read_after`) or, for a draining probe (whose rows run the drain's ops), every column
/// of the build's row.
fn reads(gx: &Gx<'_, '_>, b: usize, (q, j): (usize, usize), attno: i16) -> bool {
    match crate::ops::hashjoin::build_side::read_after(gx, &gx.pipelines()[q], j, INNER_VAR) {
        Some(p) => p.contains(&attno),
        None => drain_of(gx, q).is_some() && gx.pipelines()[b].ops.iter().rev().find_map(|op| if let Op::Project(l) = op { Some(l.len()) } else { None }).is_some_and(|n| attno > 0 && attno as usize <= n),
    }
}

/// The drain pipeline whose ops probe pipeline `q`'s rows run (a RIGHT / FULL / RIGHT SEMI / RIGHT ANTI join: `q` closes
/// into the spool `m` the drain's op names, drain/lower.rs's tail): the later pipeline that opens with a Drain of `q`'s `m`.
fn drain_of(gx: &Gx<'_, '_>, q: usize) -> Option<usize> {
    gx.pipelines().iter().enumerate().skip(q + 1).find_map(|(d, p)| match p.ops.first() {
        Some(Op::Join(JoinDesc::Drain { matches, .. })) => (gx.life_of(d, *matches) == Some(LifeId(q as u16))).then_some(d),
        _ => None,
    })
}

/// The build whose records op `j` of pipeline `k` hands on, with its pair (`pair`): a probe's, the build it pairs with; a
/// drain's, the build its source reads, whose probe's rows this pipeline continues.
pub(crate) fn records_of(gx: &Gx<'_, '_>, k: usize, j: usize) -> Option<(usize, (usize, usize))> {
    match &gx.pipelines()[k].ops[j] {
        Op::Join(JoinDesc::Probe { slot, .. }) => {
            let b = gx.life_of(k, *slot)?.0 as usize;
            (pair(gx, b) == Some((k, j))).then_some((b, (k, j)))
        }
        Op::Join(JoinDesc::Drain { .. }) if j == 0 => {
            let Source::Breaker { slot } = gx.pipelines()[k].source else { return None };
            let b = gx.life_of(k, slot)?.0 as usize;
            let pr = pair(gx, b)?;
            (drain_of(gx, pr.0) == Some(k)).then_some((b, pr))
        }
        _ => None,
    }
}

/// Build `b`'s payload columns its probe pipeline's end (a draining join's: its drain pipeline's) keys by their Fp128 (a
/// group's keys there or, multi-hop, the columns the next build carries) that have it at hand (`sourced`), under the cost
/// guard (the same guard files once); and the rows the planner expects to reach the group.
fn carried(gx: &Gx<'_, '_>, b: usize) -> (FpCols, u64) {
    crate::stack_check();
    if !JoinRoad::PayloadFp.on() {
        return (FpCols::NONE, 0);
    }
    let Some((q, j)) = pair(gx, b) else { return (FpCols::NONE, 0) };
    let e = drain_of(gx, q).unwrap_or(q);
    if e != q && !JoinRoad::DrainFp.on() {
        return (FpCols::NONE, 0); // join.drain.fp off: a draining join's build keeps the render on both of its paths
    }
    let next = crate::frame::iface::breaker_of(&gx.pipelines()[e].desc).is_some_and(|br| br.kind == BreakerKind::HashBuild);
    if next && e != q {
        return (FpCols::NONE, 0); // a drain's build is the seeded drain's Arms run (its probe's copy and the drain): build.rs `live`
    }
    let (want, rows) = if next { carried(gx, e) } else { wants(gx, e) };
    let (mut out, mut hashed) = (FpCols::NONE, false);
    for &(col, bp) in want.cols() {
        let Some(From::Inner { j: at, attno }) = walk(gx, e, end_pos(gx, e), col) else { continue };
        let Some(how) = sourced(gx, b, attno, bp).filter(|_| records_of(gx, e, at).is_some_and(|r| r.0 == b) && reads(gx, b, (q, j), attno)) else { continue };
        out.add((attno, bp));
        hashed |= how == Held::Plain;
    }
    if out.n == 0 || gravity_lower::inputs::key::decide(b, true, gx.pipelines()[b].desc.build_rows, pays, [rows, permille(gx, hashed), 0, 0]) == 0 {
        return (FpCols::NONE, rows);
    }
    (out, rows)
}

/// How a column of a build's row has its Fp128 at hand (`sourced`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Held {
    /// A code lane of its scan: the lane's table entry.
    Code,
    /// A plain text value of its scan (`plain`): its bytes' Fp128, hashed once a build row.
    Plain,
    /// Multi-hop: the words the record of a build it probes (or drains) carries.
    Carried,
}

/// How column `attno` of build pipeline `b`'s row has its Fp128 at hand: a code lane of its scan (`lane`, the table the
/// source binds), a plain text value of its scan (`plain`) or, multi-hop, a payload column a build `b` probes (or drains)
/// carries (its record's words); `None` when it has none.
fn sourced(gx: &Gx<'_, '_>, b: usize, attno: i16, bp: bool) -> Option<Held> {
    crate::stack_check();
    match walk(gx, b, end_pos(gx, b), attno) {
        Some(From::Scan(_)) if lane(gx, b, attno, bp).is_some() => Some(Held::Code),
        Some(From::Scan(_)) => plain(gx, b, attno, bp).then_some(Held::Plain),
        Some(From::Inner { j, attno: a0 }) => records_of(gx, b, j).filter(|&(b0, pr)| reads(gx, b0, pr, a0) && sourced(gx, b0, a0, bp).is_some()).map(|_| Held::Carried),
        None => None,
    }
}

/// Whether payload column `attno` of build pipeline `b`'s row is a plain text value of its scan (text or varchar, a
/// non-bpchar key) that no code lane holds: a merged pgrc scan's datum lane (decompose scan/mod.rs `merge_lanes`) or a heap
/// column. The build stores its bytes' Fp128 (rep.varlena.hash's image, build.rs `fp`), the words a code of the same bytes
/// keys by (rule A), where the statement's text keys image as that Fp128 (`plain_image`).
fn plain(gx: &Gx<'_, '_>, b: usize, attno: i16, bp: bool) -> bool {
    let Some(From::Scan(a)) = walk(gx, b, end_pos(gx, b), attno) else { return false };
    let Some((_, relid, _)) = gx.pipelines()[b].source.relation() else { return false };
    !bp && gx.facts.att(relid, a).is_some_and(|att| gravity_fn::typesem::text_payload(att.typid))
}

/// Whether a statement with strategies `s` images a plain text key as its bytes' exact Fp128 (rep.varlena.hash's last
/// form, rep/varkey.rs `hash_image`): no varlena key road that answers first applies (key.surrogate, key.canon, key.tagged:
/// a tagged image of a long text is inexact, so a build could not store it as the key's words). The build stores a plain
/// value's words only then (build.rs `fp`).
pub(crate) fn plain_image(s: crate::face::strategy::StrategySet) -> bool {
    use crate::face::strategy::{KEY_CANON, KEY_SURROGATE, KEY_TAGGED};
    !(s.has(KEY_SURROGATE) || s.has(KEY_CANON) || s.has(KEY_TAGGED))
}

/// The lane of build pipeline `b`'s scan that payload column `attno` of its row is, when it is a code lane of the right
/// bpchar kind (`bp`: the key is bpchar; then the column is a bpchar of declared length).
pub(crate) fn lane(gx: &Gx<'_, '_>, b: usize, attno: i16, bp: bool) -> Option<u8> {
    let Some(From::Scan(a)) = walk(gx, b, end_pos(gx, b), attno) else { return None };
    let (_, relid, site) = gx.pipelines()[b].source.relation()?;
    let att = gx.facts.att(relid, a)?;
    let col_bp = crate::dict::coderep::sk_bpchar(gx, att.typid) != 0;
    if col_bp != bp || (bp && att.typmod < 0) {
        return None;
    }
    let i = site.lanes.iter().position(|l| l.id == LaneId::Attr(a) && matches!(l.rep, Rep::Code { .. }))?;
    u8::try_from(i).ok()
}

/// The INNER_VAR attnos projection op `j` of pipeline `k` passes on as the record's cells: the carried columns (`cols`) of
/// the build whose records the op right before `j` hands on (a probe, or a drain: the same answer for the drain
/// pipeline's ops made as its probe's tail; D4603: or a union arm's copy of the pair's probe, `arm_copy`).
pub(crate) fn forwards(gx: &Gx<'_, '_>, k: usize, j: usize) -> FpCols {
    let ops = &gx.pipelines()[k].ops;
    let Some(i) = ops[..j].iter().rposition(|op| !matches!(op, Op::Filter(_))) else { return FpCols::NONE };
    records_of(gx, k, i).or_else(|| arm_copy(gx, k, i)).map_or(FpCols::NONE, |(b, _)| cols(gx, b))
}

/// D4603: probe op `j` of pipeline `k` as another arm of one union run copied the pair's probe `(q, i)` (decompose
/// append.rs copies the union's ops past each arm's `Arm` marker; spill/build.rs `arm_run`): it hands on the same records
/// with the words linked (walk.rs `joined`), so every arm forwards the same payload cells and the arms' rows keep one form
/// (a build past them that spills keeps one kept-row layout, spill/pair.rs D2084: an arm that rendered broke it, D4603).
fn arm_copy(gx: &Gx<'_, '_>, k: usize, j: usize) -> Option<(usize, (usize, usize))> {
    let Op::Join(JoinDesc::Probe { slot, .. }) = &gx.pipelines()[k].ops[j] else { return None };
    let b = gx.life_of(k, *slot)?.0 as usize;
    let (q, i) = pair(gx, b)?;
    let past = |p: usize, at: usize| gx.pipelines()[p].ops[..at].iter().rposition(|o| crate::graph::arm_at(o).is_some()).map(|m| at - m);
    let run = |p: usize| crate::ops::hashjoin::spill::build::arm_run(gx, p);
    let copy = q != k && past(q, i).is_some() && past(q, i) == past(k, j) && run(q).is_some() && run(q) == run(k);
    (copy && !may_swap(gx, b, k, j)).then_some((b, (q, i)))
}

/// The lanes of pipeline `k`'s scan whose fingerprint tables its build's records read (`cols`), for the source to declare
/// (each a column of the set).
pub(crate) fn lanes(gx: &Gx<'_, '_>, k: usize) -> FpCols {
    let mut out = FpCols::NONE;
    for l in cols(gx, k).cols().iter().filter_map(|&(a, bp)| lane(gx, k, a, bp)) {
        out.add((l as i16, false));
    }
    out
}
