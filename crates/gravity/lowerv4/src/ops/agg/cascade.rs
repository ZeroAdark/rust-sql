//! D4620 (road `agg.gsets.cascade`, on by default): a grouping set folded from a finer set's records. Decompose's
//! `gsets_tag::cascade` (form T, ops/agg/gsets_tag.rs) folds each input row once, under the sets no requested set is a
//! strict superset of (the base: form T's tagged group), and declares every other set as a group of the base's record
//! (`PipelineGraph::cascades`) whose producer reads its candidate parents' owned records. Here that producer's source is
//! one row per claim (kind `keyed.gsets`: the claims are the parent's owner runs, `ClaimKind::OwnerRuns`, its nominal
//! site the base producer's claims site) and its end is the hashed table of the base's record, into which the kernels'
//! `agg_cascade` folds the claim's share of the parent with the fewest records: each record of the parent's set, its tag
//! the set's own and every column the set does not group NULL, merged by `Rec::fold` (the owner phase's merge). Each
//! cascaded group then runs its owner phase as any hashed group; the base's emit (table/emit_group.rs) reads its records
//! after its own (`extras`), so its tag-keyed lists (form T's CASE over the tag) emit every set.
//!
//! The base's end takes none of the forms that keep only some of its groups (a per-code front with its HAVING floor, a
//! parted or count-ordered owner, a HAVING split, an admitted domain, a code cut, a walked skip, the owner's count floor:
//! hashagg.rs `make` / `end_of` read `base`): the cascades read every group the base folds.
use gravity_abi::breaker::KeyKind;
use gravity_abi::hash::Depth;
use gravity_decompose::graph::Source;
use gravity_decompose::ops::shared::agg::{Cascade, GroupDecl, NONE};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, HostFlags, IntBinOp, IntCond, Type};
use gravity_program::{ClaimKind, LifeId, Parallel, Pipeline, Reason as Why};
use mcx::{GrowIn, PgBox, PgVec};

use crate::admit::refusal::{Reason, ShapeWhy};
use crate::cell::Row;
use crate::face::end::EndMech;
use crate::face::source::{SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::Win;
use crate::frame::open::OpenWords;
use crate::frame::px::Px;
use crate::frame::resume::{resume_pos, write_pos};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::table::bypass::HASHED;
use crate::table::face::{Domain, KeyShape, TableDecl, TableMech};
use crate::table::rec::GroupRec;

crate::strategies! {
    /// D4620: coarser grouping sets folded from a finer set's owned records (decompose ops/agg/gsets_tag.rs `cascade`, read
    /// through `Facts::default_on_road_on`; this file lowers what it declares). On by default since its flip; off (a plain
    /// `agg.gsets.cascade`), form T fans every row out to every set.
    GSETS_CASCADE = 157, "agg.gsets.cascade", Keyed;
}

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// The cascaded set pipeline `k` produces (decompose `PipelineGraph::cascades`).
pub(crate) fn at<'g>(gx: &Gx<'g, '_>, k: usize) -> Option<&'g Cascade> {
    let graph: &'g gravity_decompose::graph::PipelineGraph<'_> = gx.graph;
    graph.cascades.iter().find(|c| c.producer.0 as usize == k)
}

/// Whether pipeline `k` produces a cascade's base (form T's tagged group the cascaded sets are folded from).
pub(crate) fn base(gx: &Gx<'_, '_>, k: usize) -> bool {
    gx.graph.cascades.iter().any(|c| c.base.0 as usize == k)
}

/// Whether group `g` is a cascade's base.
pub(crate) fn base_group(gx: &Gx<'_, '_>, g: &GroupDecl<'_>) -> bool {
    base(gx, g.producer.0 as usize)
}

/// The consumed regions of pipeline `k`, an emit of group `g`, that hold `g`'s cascaded sets' groups, in their order:
/// `None` where `g` is no cascade's base. A drain of the emit's array (`drain`) gathers nothing again: none.
pub(crate) fn extras<'g>(gx: &Gx<'g, '_>, k: usize, g: &GroupDecl<'_>, drain: bool) -> Option<PgVec<'g, u64>> {
    if drain || !base_group(gx, g) {
        return None;
    }
    let Some(p) = gx.pipelines().get(k) else { shape(-1, "a cascaded grouping set's emit inside an inner (D4620)") };
    let node = p.node_id;
    let mut v = PgVec::new_in(gx.mcx);
    for c in gx.graph.cascades.iter().filter(|c| c.base == g.producer) {
        let at = gx.pipelines().consumed_at(k, c.slot).filter(|_| gx.life_of(k, c.slot) == Some(c.producer));
        let Some(i) = at else { bug(node, "a cascade base's emit that does not read a cascaded set's group (D4620)") };
        v.push_in(i as u64);
    }
    if v.len() > gravity_kernels::host_abi::EMIT_EXTRAS_MAX {
        shape(node, "more cascaded grouping sets than the emit gathers (D4620)");
    }
    Some(v)
}

/// The cascaded set's source: one row per claim, the end folding the claim's share of the parent (`agg_cascade`).
struct Src {
    site: Option<usize>,
    rows: u64,
}

pub(crate) fn make_source<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let node = gx.pipelines()[k].node_id;
    let Some(c) = at(gx, k) else { bug(node, "a cascaded grouping set's source with no cascade (D4620)") };
    if !matches!(gx.pipelines()[k].source, Source::Breaker { .. }) {
        bug(node, "a cascaded grouping set's source that is not its parent's breaker (D4620)");
    }
    // the parents' records are sealed before this pipeline runs: its claims are their owner runs, the base producer's
    // claims site nominal (no row is read through it); a producer with none keeps the one call on the leader
    let site = px.claims_site(c.base.0 as usize);
    mcx::pg_box_dyn!(px.mcx(), Src { site, rows: gx.pipelines()[k].desc.predicted_rows } => dyn SourceMech + 'g)
}

impl SourceMech for Src {
    fn open(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env) -> gravity_ir::Value {
        env.zero
    }

    /// One row per call: the claim (the end folds its whole share at the row).
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let at = resume_pos(b, env);
        let (done, l_go) = (b.icmp(IntCond::Sge, at, env.one), b.create_block(&[]));
        b.cond_br(done, (l_eof, &[env.zero]), (l_go, &[]));
        b.switch_to(l_go);
        Win { cursor: at, end: env.one, facts: WinConds::default() }
    }

    fn row<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: gravity_ir::Value, carry: Carry<'e>, next: gravity_ir::Value) -> RowVals<'e> {
        RowVals { row: Row::new_in(env.mcx), vals: PgVec::new_in(env.mcx), cols: PgVec::new_in(env.mcx), carry, next, at: j }
    }

    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: gravity_ir::Value, _out: gravity_ir::Value) {
        write_pos(b, env, cur);
    }

    fn nominal_site(&self, _px: &Px<'_, '_>) -> Option<usize> {
        self.site
    }

    /// Wide over the parents' owner runs (`ClaimKind::OwnerRuns`), at its share of the planner's groups; marked a cascaded
    /// set (`Pipeline::cascade`: its region starts small, its owners regrow to their even share first).
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        let d = px.gx.pipelines()[k].desc;
        match self.site {
            Some(s) => {
                fires::fire("gsets.cascade.wide"); // shapecheck fire signal (D4620): a cascaded set folds its parent's runs by claims
                Pipeline { parallel: Parallel::Eligible, driving_site: s as i16, predicted_rows: self.rows, claims: ClaimKind::OwnerRuns, cascade: true, ..d }
            }
            None => Pipeline { parallel: Parallel::Ineligible { site: d.driving_site, reason: Why::NoDrivingSite }, ..d },
        }
    }
}

/// The cascaded set's end: the hashed table of the base's record (`t`), `agg_cascade` folding the claim's share of the
/// parent into it under `desc`'s words (kernels fold_entries.rs `CASCADE_*`).
struct End<'g> {
    t: TableDecl,
    desc: PgVec<'g, u64>,
    resident: bool,
}

pub(crate) fn make_end<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let gx = px.gx;
    let node = gx.pipelines()[k].node_id;
    let Some(c) = at(gx, k) else { bug(node, "a cascaded grouping set's end with no cascade (D4620)") };
    let Some(g) = gx.group_of(c.base) else { bug(node, "a cascaded grouping set whose base declares no group (D4620)") };
    let rec = GroupRec::new(px, g);
    // decompose declares the cascade over a record the owners merge alone (`gsets_tag::map`): no hosted state, no
    // order-keyed extreme, the tag a one-word integer key 0
    let tagged = matches!(g.keys.first().map(|kd| kd.kind), Some(KeyKind::I32 | KeyKind::I64)) && rec.wpk.first() == Some(&1);
    if rec.handle.is_some() || rec.layout.slot_kinds().contains(&gravity_abi::breaker::SlotKind::AggState) || rec.layout.flags & gravity_abi::breaker::LAYOUT_KEY_TIES != 0 || rec.dep.is_some() || !tagged {
        shape(node, "a cascaded grouping set over a record the owners' merge does not take (D4620)");
    }
    let nk = rec.layout.nkeys();
    let (mut and, mut or) = (mcx::pg_vec![in px.mcx(); u64::MAX; nk], mcx::pg_vec![in px.mcx(); 0u64; nk]);
    let tw = rec.key_words(0).0;
    (and[tw], or[tw]) = (0, c.tag as u32 as u64);
    let mut reps: PgVec<'_, u64> = PgVec::new_in(px.mcx());
    for &i in c.nulls.iter() {
        let i = i as usize;
        let (first, n) = rec.key_words(i);
        let Some((fw, bit)) = rec.null_flag(i) else { bug(node, "a cascaded set's NULL key in a record without flags (D4620)") };
        if first + n > nk || fw >= nk {
            bug(node, "a cascaded set's NULL key past the record's key words (D4620)");
        }
        for w in first..first + n {
            (and[w], or[w]) = (0, 0);
        }
        or[fw] |= 1u64 << bit;
        if g.keys[i].rep != NONE {
            reps.push_in(g.keys[i].rep as u64);
        }
    }
    let mut desc: PgVec<'g, u64> = PgVec::new_in(px.mcx());
    desc.extend_in([c.parents.len() as u64, tw as u64, reps.len() as u64]);
    for p in c.parents.iter() {
        let Some(i) = gx.pipelines().consumed_at(k, p.slot) else { bug(node, "a cascaded set's parent its pipeline does not read (D4620)") };
        desc.extend_in([i as u64, p.tag as u32 as u64]);
    }
    desc.extend_in(and.iter().copied());
    desc.extend_in(or.iter().copied());
    desc.extend_in(reps.iter().copied());
    debug_assert_eq!(desc.len(), gravity_kernels::host_abi::cascade_desc_words(c.parents.len(), nk, reps.len()));
    // the base's record, hashed; the key copies its records name are the base's (no compact reclaims them here)
    let mut t = TableDecl { image_words: nk as u16, record_bytes: rec.layout.size() - 8 * nk as u32, expect: None, depth: Depth::TOP, life: LifeId(k as u16), layout: rec.layout, state_off: 0, key: KeyShape { code: None, dense: None, domain: Domain::Unknown }, facts: 0, copies: 0 };
    t.facts = crate::table::size::declare_facts(px, k, &t, 0);
    let resident = (0..rec.reps.len()).any(|i| crate::table::emit_key::copies(&rec, i));
    fires::fire("gsets.cascade.end"); // shapecheck fire signal (D4620): a cascaded set's table of its parent's records
    mcx::pg_box_dyn!(px.mcx(), End { t, desc, resident } => dyn EndMech + 'g)
}

impl EndMech for End<'_> {
    fn state_bytes(&self) -> u32 {
        HASHED.state_bytes(&self.t)
    }

    fn open_words(&self, w: &mut OpenWords) {
        HASHED.open_words(&self.t, w);
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        HASHED.open(b, env, &self.t);
    }

    /// The claim's row: its share of the parent folded into the table (`agg_cascade`), the bypass word read again (the fold
    /// may have switched the table to its partitions).
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, x: &Exits) -> Carry<'e> {
        let st = crate::table::bypass::state(b, env, &self.t);
        let slot = b.declare_stack_slot(8 * self.desc.len() as u32, 8);
        let d = b.stack_addr(slot);
        for (i, &w) in self.desc.iter().enumerate() {
            let v = b.const_raw(Type::I64, w);
            b.store(d, 8 * i as i32, v);
        }
        let h = crate::table::bypass::host(b, "agg_cascade", &[Type::Ptr, Type::Ptr, Type::Ptr], gravity_abi::host_words!(gravity_kernels::host_abi::agg_cascade(ctx, st, desc)));
        let r = b.call_host(h, &[env.ctx, st, d], Some(Type::I64)).expect("agg_cascade returns a status");
        crate::table::bypass::fail_on(b, env, x, r);
        crate::table::bypass::sync_bypass(b, st);
        Carry::new(b.int_bin(IntBinOp::Add, row.carry.out, env.one), env.mcx)
    }

    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _c: &Carry) {
        HASHED.window_end(b, env, &self.t);
    }

    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        HASHED.finish(b, env, &self.t, x);
    }

    /// The owner phase merges the base's record as the table holds it; a record holding key copies never spills (its
    /// table grows its grant, as the base's: table/emit_key.rs `resident`).
    fn pipeline_desc(&self, _px: &Px<'_, '_>, _k: usize, mut d: Pipeline) -> Pipeline {
        d.words = None;
        if self.resident {
            d = crate::frame::iface::end_resident(d, gravity_program::spill::NoSpill::KeyCopies);
        }
        if let Some(o) = d.owner.as_mut() {
            (o.layout, o.acc) = (self.t.layout, self.t.layout);
            o.observe_key = gravity_program::OBSERVE_NONE;
        }
        d
    }
}
