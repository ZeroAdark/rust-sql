//! The runner's `Plan` for the driver: per-pipeline widths, claims (morsel sources), merges and the after-pipeline hook.
//! Split out of runner.rs (move-only, lane SPLIT 09-23).

use super::*;
use mcx::PgVec;

pub(super) enum Claims {
    None,
    Blocks(BlockRange),
    Granules(GranuleRange),
    /// The spill partitions of a breaker slot's stream (`ClaimKind::SpillPartitions`), sized once its producer ran (`after_pipeline`).
    Partitions(gravity_parallel::PartitionRange, u8),
    /// D3024 (b): a group emit's slices of its table's owner runs (`ClaimKind::OwnerRuns`): a fixed count
    /// (`abi::breaker::OWNER_RUN_SLICES`), cut at prepare and never sized by a producer, so every path that runs the emit
    /// (a section, the width-1 arm, a lazy build's or a pull's) claims the same groups; the kernel maps slices to runs.
    Slices(gravity_parallel::PartitionRange),
}

pub(super) struct NoPool;

impl Pool for NoPool {
    fn run(&self, _width: u16, _claims: &dyn MorselSource, _job: &Job<'_>) -> core::result::Result<(), FirstError> {
        unreachable!("gravity: a section without a pool")
    }
}

const PLAIN: PlainAggPartial = PlainAggPartial;

const ROWS: RowsUnordered = RowsUnordered;

/// The runner's answers per pipeline (`gravity_parallel::Plan`): the width, the claims over the pipeline's driving relation resolved for this
/// execution (block ranges of a heap, granule ranges of a pgrc part list), the merge per breaker, and the leader's post-section ordering.
/// D133: the wall a build meets when it outgrows its drawn share with no part phase behind it (named, never an empty probe).
const SPILLED_UNREAD: &str = "gravity: a hash build spilled past its share with no part phase to read its partitions (M7 L4: the spilled build is chosen at the claim, D133)";

pub(super) struct RunnerPlan {
    pub(super) program: Arc<Program>,
    /// D2736: each pulled pipeline's late key words since its first pull (`Plan::pulled_late`), until a re-open.
    pulled: core::cell::RefCell<Vec<(usize, gravity_parallel::LateWords)>>,
    /// The width per pipeline: the grant for an eligible pipeline, 0 for a serial one.
    pub(super) widths: PgVec<'static, u16>,
    claims: Vec<Claims>,
    /// The `Claims::Partitions` entries of `claims` as (pipeline, slot), in pipeline order: `after_pipeline` sizes them per
    /// producer without a walk of every pipeline's claims.
    partitions: PgVec<'static, (usize, u8)>,
    /// D3024 (b), sorted: whether the program declares a sorted wide emit (`ProtoKind::EmitRuns`), and per pipeline (a bit
    /// of 256) whether such an emit still to run stays on the leader: `after_pipeline` sets the bit once its producer's
    /// groups fit the owner rule's leader bytes (`emit_on_leader`), and `width_of` then runs the emit's width-1 arm over
    /// the same slices (the leader's runs, merged as a section's) instead of a section that would wake the pool for them.
    sorted_emits: bool,
    emit_leader: [Cell<u64>; 4],
    /// D3024 (b): whether a pipeline's claims are `ClaimKind::BuildRecords` (a build drain's), and per build slot (a bit
    /// of 256) whether the drain of that slot's build still to run stays on the leader: `after_pipeline` sets the bit
    /// once the build's records fit the owner rule's leader bytes (`drain_on_leader`), and `width_of` then runs the
    /// drain's width-1 arm over the same slices instead of a section that would wake the pool for them.
    drains: bool,
    drain_leader: [Cell<u64>; 4],
    /// The HashAgg is keyed by (part, code) of a dictionary lane: the leader crosses parts by value.
    text_groups: bool,
    hash_build: HashBuildPartial,
    /// The HashAgg merge per pipeline (§2.6 item 16): its declared owner phase over its own words, else the program's declared one.
    /// D3586: only the pipelines that end in a HashAgg or Distinct breaker (`merge_for` reads no other), by pipeline.
    hash_aggs: PgVec<'static, (usize, HashAggPartial)>,
    two_pass: bool,
    /// The two-pass form's exporting and admitting pipelines (the declared `two`'s).
    pub(super) pass1: usize,
    pass2_idx: usize,
    pub(super) two: Option<gravity_program::TwoPassDesc>,
    /// Pass 2 over the admitted granules alone (`marks`): set after pass 1 when the marks cut the walk; `pass2_rel` its
    /// relation, `pass2_site` its driving site (the order's key).
    pub(super) pass2: pgsync::OnceLock<GranuleRange>,
    pub(super) pass2_rel: u32,
    pub(super) pass2_site: usize,
    /// The census: pass 2's admitted granules over the relation's (0 / 0 without the cut), and the keys pass 1 exported.
    pub(super) admitted: Cell<(u64, u64)>,
    pub(super) exported: Cell<u64>,
    top_k: TopKPartial,
    /// The zone walk's plan: `groups` cuts the ordered table at n, rows come from the TopK run.
    walk: Option<Walk>,
    /// A declared code run's lanes (`TopKDesc.code_lanes`): the site lane of each code column; `None`: the column is the lane (the shape table's runs).
    topk_lanes: Option<u64>,
    pub(super) sections: Cell<u32>,
    /// D3014 S2': `PGRUST_GRAVITY_RERUN_SECTIONS=0` (read at prepare) keeps every rerun member serial.
    rerun_sections: bool,
    /// The binding's parameter words: the statement's key params, then the kernel words the
    /// runner derives (`par_hashagg::NPARAMS`: the top-k bound, the dense domain, the spill bits).
    pub(super) params: PgVec<'static, u64>,
    pub(super) nulls: PgVec<'static, u8>,
    pub(super) nparams: usize,
    /// The program's plan key for the memo's group count (partitioned-record.md §3.12 item 5): a declared owner's.
    key: Option<PlanKey>,
    /// The dense key domain the group-by kernel was handed (`None`: hashed only), for the census.
    dense: Option<(i64, i64)>,
    /// The statement's granule relations, resolved once here on the leader and read by every
    /// source's bind through the binding (§4.2); boxed so the sources' pointer to it stands.
    pub(super) resolved: Box<granule_source::Resolved>,
    /// The statement's toast relations for the pool's claims (D151), resolved here on the leader when a section may run.
    pub(super) toast: Option<Arc<heaptoast::worker::ToastMap>>,
    pub(super) v4: crate::ops::State,
    /// D899: this execution's late key words as (pipeline, word, key expression), each bound when its pipeline starts
    /// (`late_words`), over the admission's key expressions (`late_exprs`, `prepare`'s `a`) in `late_mcx`.
    late: PgVec<'static, (usize, usize, usize)>,
    late_exprs: *mut crate::shapes::KeyExprs,
    late_mcx: Mcx<'static>,
    /// The pipelines driven by a bitmap morsel site, each built as it starts (`late_words`).
    bitmaps: Vec<BitmapRun>,
    /// The pipelines driven by an index-only site the pool reads by leaf pages, each run's walk captured as it starts.
    leaves: Vec<LeafRun>,
    /// The pipelines driven by an ordered index site the pool reads in key ranges, each run's ranges cut as it starts.
    ranges: Vec<RangeRun>,
    snap: Rc<SnapshotData<'static>>,
    /// `prepare`'s own vectors, carried only to go back with the plan's (`give_back`).
    spare: Spare<'static>,
}

/// The vectors a plan sizes per pipeline and per word, kept between a level's parallel runs in its "runner plan scratch"
/// context (`Owner::Session`, a named context row): `run_parallel` takes them for its plan and gives them back at its end,
/// so a statement reuses their room instead of allocating and freeing each. A portal's plan (`Producers`) places its own
/// in the portal's context. Drained at the idle release, with a freed deep level and with the level (the State phase);
/// a context past `PLAN_SCRATCH_MAX_BYTES` (abi limits) is not kept. `claims` stays a heap vector: its claims hold their own bounds.
pub struct PlanScratch {
    /// Before `root`: fields drop in order, and every vector here was placed through `root`'s held handle.
    bufs: Option<Bufs<'static>>,
    root: Option<gravity_driver::mem::root::Root>,
}

pub(super) struct Bufs<'m> {
    widths: PgVec<'m, u16>,
    words: PgVec<'m, u64>,
    word_nulls: PgVec<'m, u8>,
    partitions: PgVec<'m, (usize, u8)>,
    hash_aggs: PgVec<'m, (usize, HashAggPartial)>,
    late: PgVec<'m, (usize, usize, usize)>,
    spare: Spare<'m>,
}

struct Spare<'m> {
    dense_of: PgVec<'m, Option<(i64, i64)>>,
    rows_of: PgVec<'m, Option<u64>>,
    cosplit: PgVec<'m, bool>,
    owners: PgVec<'m, (usize, OwnerFns, Option<OwnerFns>)>,
}

impl<'m> Bufs<'m> {
    pub(super) fn new(mcx: Mcx<'m>) -> Bufs<'m> {
        let spare = Spare { dense_of: mcx::vec_new_in(mcx), rows_of: mcx::vec_new_in(mcx), cosplit: mcx::vec_new_in(mcx), owners: mcx::vec_new_in(mcx) };
        Bufs { widths: mcx::vec_new_in(mcx), words: mcx::vec_new_in(mcx), word_nulls: mcx::vec_new_in(mcx), partitions: mcx::vec_new_in(mcx), hash_aggs: mcx::vec_new_in(mcx), late: mcx::vec_new_in(mcx), spare }
    }
}

impl PlanScratch {
    pub const fn new() -> PlanScratch {
        PlanScratch { bufs: None, root: None }
    }

    pub(super) fn take(&mut self) -> Bufs<'static> {
        if let Some(b) = self.bufs.take() {
            return b;
        }
        let root = self.root.get_or_insert_with(|| {
            let r = gravity_driver::mem::root::Root::start_small("runner plan scratch");
            r.mcx().context().set_owner_session();
            r
        });
        Bufs::new(super::raw::scratch_mcx(root))
    }

    /// `b` (from `take`) kept, emptied, for the level's next plan while the context holds at most `PLAN_SCRATCH_MAX_BYTES`.
    pub(super) fn give(&mut self, mut b: Bufs<'static>) {
        if self.bufs.is_some() || self.root.as_ref().is_none_or(|r| r.used() > gravity_abi::limits::PLAN_SCRATCH_MAX_BYTES) {
            core::mem::drop(b);
            if self.bufs.is_none() {
                self.drain();
            }
            return;
        }
        b.widths.clear();
        b.words.clear();
        b.word_nulls.clear();
        b.partitions.clear();
        b.hash_aggs.clear();
        b.late.clear();
        b.spare.dense_of.clear();
        b.spare.rows_of.clear();
        b.spare.cosplit.clear();
        b.spare.owners.clear();
        self.bufs = Some(b);
    }

    /// The kept vectors and their context gone: the bytes off the session's row.
    pub fn drain(&mut self) {
        self.bufs = None;
        self.root = None;
    }
}

impl Default for PlanScratch {
    fn default() -> PlanScratch {
        PlanScratch::new()
    }
}

/// D4542: a plan that goes back to its level's scratch however its run ends (`run_parallel`): a cancelled or failed run
/// gives its vectors back as a finished one does, so the scratch's context holds after an aborted statement what it held
/// before it. Dropped instead, an aborted plan's vectors went to the context's free lists and the next plan's fresh ones
/// took a new block (v4/m5_cancel_pool: 'runner plan scratch' 19456 -> 35840 bytes across one cancel).
pub(super) struct Planned<'s> {
    plan: Option<RunnerPlan>,
    scratch: &'s mut PlanScratch,
}

impl<'s> Planned<'s> {
    pub(super) fn new(plan: RunnerPlan, scratch: &'s mut PlanScratch) -> Planned<'s> {
        Planned { plan: Some(plan), scratch }
    }

    pub(super) fn plan(&mut self) -> &mut RunnerPlan {
        self.plan.as_mut().expect("a Planned holds its plan until its drop")
    }
}

impl Drop for Planned<'_> {
    fn drop(&mut self) {
        if let Some(p) = self.plan.take() {
            p.give_back(self.scratch);
        }
    }
}

/// `v` emptied and `n` long of `f()`, its room reserved fallibly.
fn sized<T>(v: &mut PgVec<'static, T>, n: usize, f: impl FnMut() -> T) -> PgResult<()> {
    v.clear();
    room(v, n)?;
    v.resize_with(n, f);
    Ok(())
}

fn room<T>(v: &mut PgVec<'static, T>, n: usize) -> PgResult<()> {
    let mcx = *v.allocator();
    v.try_reserve(n).map_err(|_| mcx.oom_boxed(n.saturating_mul(core::mem::size_of::<T>())))
}

/// D3014O: every site a worker probes (`SiteDesc::worker_probe`) reads this execution's frozen image of its index, captured
/// here on the leader (`WorkerProbe::capture`) and filed in this execution's `Resolved` by registry entry, where each
/// worker's filter binds it (D3117: never in the cached recipe, which other statements' executions share); `false` when
/// one does not hold (not imageable, a function not pool-hosted, SERIALIZABLE, or a recipe without its worker form): P3,
/// the execution runs its pipelines on the leader, whose own index source serves the site, so no worker meets a probe
/// without its image. The count filed is `filed`.
fn capture_probes(mcx: Mcx<'_>, a: &Admitted, resolved: &mut granule_source::Resolved, filed: &mut u32) -> PgResult<bool> {
    let mut held = true;
    for (site, d) in a.program.sites.iter().enumerate() {
        if !d.worker_probe() {
            continue;
        }
        // the worker's heap read publishes the attribute lanes and a last RowId lane (hosts index_probe.rs)
        let attrs = gravity_hosts::ops::scan::index_probe::lanes_served(d);
        let Some(p) = a.registry.worker_probe(site).filter(|_| attrs) else {
            held = false;
            continue;
        };
        let index = indexam::index_open(mcx, p.index, types_storage::lock::NoLock)?;
        let got = p.capture(&index, xact::TransactionStartedDuringRecovery());
        index.close(types_storage::lock::NoLock)?;
        match got? {
            Some(f) => {
                resolved.file_probe(site, f);
                *filed += 1;
            }
            None => held = false,
        }
    }
    Ok(held)
}

/// A bitmap morsel pipeline's run: its recipe, the site's key words (`param_slots`; -1 a host key),
/// the execution's width and, once built, the width the expanded pages give (the heap rule: one worker per `BLOCK_C0`
/// pages up to the grant, the leader alone under two).
struct BitmapRun {
    pipeline: usize,
    recipe: gravity_hosts::ops::scan::bitmap_morsel::BitmapMorselRecipe,
    slots: Vec<i16>,
    granted: u16,
    width: Cell<Option<u16>>,
    /// A2 entry 2: the pool threads a btree build may walk the leaves on (`leaf_width`; 0: the leader).
    leaves: u16,
}

/// A2 entry 2: the leaf walk's threads for an index estimate of `rows` at grant `width`: one per
/// `INDEX_LEAF_ROWS_PER_WORKER`, up to the grant; under two the leader walks (0). A forced width is taken as granted.
fn leaf_width(rows: u64, width: u16) -> u16 {
    if forced_width().is_some() {
        return if width >= 2 { width } else { 0 };
    }
    match (rows / gravity_program::cost::INDEX_LEAF_ROWS_PER_WORKER).min(width as u64) {
        0 | 1 => 0,
        w => w as u16,
    }
}

impl Drop for BitmapRun {
    fn drop(&mut self) {
        self.recipe.release();
    }
}

/// A pipeline driven by an index-only site the pool reads by leaf pages (`SiteDesc::leaves`, hosts index_leaves.rs): its
/// index recipe, the site's key words (`param_slots`), the leaf walk's width (`leaf_width` of the grant), the execution's
/// slot of the site (filed in `Resolved`, where every participant's source reads it) and, once a run started, the width
/// the run's capture gives: the leaf width when it held, else 0 (P3: the leader's serial walk, the slot empty).
struct LeafRun {
    pipeline: usize,
    recipe: gravity_hosts::ops::scan::index_source::SiteRecipe,
    slots: Vec<i16>,
    granted: u16,
    cell: Arc<gravity_hosts::ops::scan::index_leaves::LeafCell>,
    width: Cell<Option<u16>>,
}

impl Drop for LeafRun {
    fn drop(&mut self) {
        // the execution's end: the last run's image and shared state go
        self.cell.set(None);
    }
}

/// A pipeline driven by an ordered index site the pool reads in key ranges (`SiteDesc::ranges`, hosts index_ranges.rs):
/// its index recipe, the site's key words (`param_slots`), the width (`leaf_width` of the grant), the execution's slot of
/// the site (filed in `Resolved`, where every participant's source reads it) and, once a run started, the width the run's
/// cut gives: the granted width when it held, else 0 (P3: the leader's serial walk, the slot empty).
struct RangeRun {
    pipeline: usize,
    recipe: gravity_hosts::ops::scan::index_source::SiteRecipe,
    slots: Vec<i16>,
    granted: u16,
    cell: Arc<gravity_hosts::ops::scan::index_ranges::RangeCell>,
    width: Cell<Option<u16>>,
}

impl Drop for RangeRun {
    fn drop(&mut self) {
        // the execution's end: the last run's image and ranges go
        self.cell.set(None);
    }
}

impl RunnerPlan {
    /// Wave 2: the claim cursors cut at prepare (part edges, dead ranges, an ordered walk's order and minima) charged to the
    /// statement's owner (Q6: no slot owner exists yet when they are cut), on the leader that cut them, to the plan's drop.
    pub(super) fn charge_claims(&self, owner: Option<memledger::Owner>) {
        // bounded("small-fixed: one entry per pipeline of the program")
        for c in &self.claims {
            match c {
                Claims::Blocks(r) => r.0.charge_to(owner),
                Claims::Granules(r) => r.0.charge_to(owner),
                Claims::Partitions(r, _) | Claims::Slices(r) => r.0.charge_to(owner),
                Claims::None => {}
            }
        }
    }

    /// Pipeline `p`'s bitmap, built once per execution from the statement's words with `late` (its runtime
    /// keys) bound, its claims sized to the frozen entries.
    fn build_bitmap(&self, p: usize, late: &[(usize, u64, u8)]) -> PgResult<()> {
        let Some(b) = self.bitmaps.iter().find(|b| b.pipeline == p) else { return Ok(()) };
        if b.width.get().is_some() {
            return Ok(());
        }
        let word = |w: usize| late.iter().rev().find(|l| l.0 == w).map(|l| (l.1, l.2)).unwrap_or_else(|| (self.params.get(w).copied().unwrap_or(0), self.nulls.get(w).copied().unwrap_or(1)));
        let (keys, nulls): (Vec<u64>, Vec<u8>) = b.slots.iter().map(|&s| if s >= 0 { word(s as usize) } else { (0, 0) }).unzip(); // alloc: Exec
        let (units, pages) = b.recipe.build(self.snap.clone(), &keys, &nulls, b.leaves)?;
        let width = if b.granted < 2 || forced_width().is_some() { b.granted } else { gravity_program::heap_width(pages, b.granted) };
        // D3183: the startup claim from the built entries (the plan's was from its row estimate, up to rows-per-page times
        // the entries), its floor `BLOCK_C0` pages in entries (a lossy chunk is one entry of up to a chunk's pages)
        if let Claims::Blocks(r) = &self.claims[p] {
            let floor = if pages == 0 { 1 } else { (BLOCK_C0 * units).div_ceil(pages).max(1) };
            r.0.set_total_aimed(units, startup_c0(units, width, floor, units.max(floor)));
        }
        b.width.set(Some(width));
        // shapecheck fire signal: `wide` only when a section runs (the pipeline eligible at width 2 or more)
        fires::fire(if width.min(self.widths[p]) >= 2 { "bitmap.morsel.wide" } else { "bitmap.morsel.serial" });
        Ok(())
    }

    /// Pipeline `p`'s leaf walk for this run, captured from the statement's words with `late` (its runtime keys) bound:
    /// put in the site's slot, where every participant's source joins it at its claim's seek. Each run captures its own
    /// (a rerun's shared scan starts over). When the capture does not hold (not imageable, a function not pool-hosted,
    /// SERIALIZABLE) or a key is NULL, the slot stays empty and the run is the leader's serial walk (width 0).
    fn build_leaves(&self, p: usize, late: &[(usize, u64, u8)]) -> PgResult<()> {
        let Some(l) = self.leaves.iter().find(|l| l.pipeline == p) else { return Ok(()) };
        let word = |w: usize| late.iter().rev().find(|x| x.0 == w).map(|x| (x.1, x.2)).unwrap_or_else(|| (self.params.get(w).copied().unwrap_or(0), self.nulls.get(w).copied().unwrap_or(1)));
        let (keys, nulls): (Vec<u64>, Vec<u8>) = l.slots.iter().map(|&s| if s >= 0 { word(s as usize) } else { (0, 0) }).unzip(); // alloc: Exec
        l.cell.set(None);
        let filed = match (l.recipe.leaves(), l.recipe.bind_keys(self.late_mcx, &keys, &nulls)?) {
            (Some(leaf), Some(bound)) => {
                let mcx = self.late_mcx;
                let index = indexam::index_open(mcx, l.recipe.index, types_storage::lock::NoLock)?;
                let heap = table::table_open(mcx, l.recipe.heap, types_storage::lock::NoLock);
                let got = match heap {
                    Ok(heap) => {
                        let got = leaf.capture(&index, &heap, &bound, &self.snap, xact::TransactionStartedDuringRecovery());
                        heap.close(types_storage::lock::NoLock)?;
                        got
                    }
                    Err(e) => Err(e),
                };
                index.close(types_storage::lock::NoLock)?;
                got?
            }
            _ => None,
        };
        let width = if filed.is_some() { l.granted } else { 0 };
        // a claim per participant and a spare, each joining the walk; the leader's serial walk is one claim (each claim of
        // it walks the whole index)
        if let Claims::Blocks(r) = &self.claims[p] {
            r.0.set_total(if filed.is_some() { l.granted as u64 + 1 } else { 1 });
        }
        l.cell.set(filed);
        l.width.set(Some(width));
        // shapecheck fire signal: `wide` when the run's walk is the pool's, `serial` when it fell back to the leader's
        fires::fire(if width.min(self.widths[p]) >= 2 { "index.only.leaves.wide" } else { "index.only.leaves.serial" });
        Ok(())
    }

    /// Pipeline `p`'s key ranges for this run, cut from the statement's words with `late` (its runtime keys) bound
    /// (`INDEX_RANGES_PER_WORKER` per participant): put in the site's slot, where every participant's source reads its
    /// claim's ranges, a claim a range. Each run cuts its own. When the cut does not hold (not imageable, a function not
    /// pool-hosted, SERIALIZABLE, a single range) or a key is NULL, the slot stays empty and the run is the leader's
    /// serial walk (width 0, one claim).
    fn build_ranges(&self, p: usize, late: &[(usize, u64, u8)]) -> PgResult<()> {
        let Some(g) = self.ranges.iter().find(|g| g.pipeline == p) else { return Ok(()) };
        let word = |w: usize| late.iter().rev().find(|x| x.0 == w).map(|x| (x.1, x.2)).unwrap_or_else(|| (self.params.get(w).copied().unwrap_or(0), self.nulls.get(w).copied().unwrap_or(1)));
        let (keys, nulls): (Vec<u64>, Vec<u8>) = g.slots.iter().map(|&s| if s >= 0 { word(s as usize) } else { (0, 0) }).unzip(); // alloc: Exec
        g.cell.set(None);
        let parts = (g.granted as usize + 1) * gravity_program::cost::INDEX_RANGES_PER_WORKER;
        let filed = match (g.recipe.ranges(), g.recipe.bind_keys(self.late_mcx, &keys, &nulls)?) {
            (Some(r), Some(bound)) => {
                let mcx = self.late_mcx;
                let index = indexam::index_open(mcx, g.recipe.index, types_storage::lock::NoLock)?;
                let heap = table::table_open(mcx, g.recipe.heap, types_storage::lock::NoLock);
                let got = match heap {
                    Ok(heap) => {
                        let got = r.capture(&index, &heap, &bound, xact::TransactionStartedDuringRecovery(), parts);
                        heap.close(types_storage::lock::NoLock)?;
                        got
                    }
                    Err(e) => Err(e),
                };
                index.close(types_storage::lock::NoLock)?;
                got?
            }
            _ => None,
        };
        let filed = filed.filter(|f| f.len() >= 2);
        let width = if filed.is_some() { g.granted } else { 0 };
        if let Claims::Blocks(r) = &self.claims[p] {
            r.0.set_total(filed.as_ref().map_or(1, |f| f.len() as u64));
        }
        g.cell.set(filed);
        g.width.set(Some(width));
        // shapecheck fire signal: `wide` when the run's ranges are the pool's, `serial` when it fell back to the leader's
        fires::fire(if width.min(self.widths[p]) >= 2 { "index.ranges.wide" } else { "index.ranges.serial" });
        Ok(())
    }
}

/// DR-6: a partitioned breaker's bits bound at run time to the machine's `slice` and to the region its phase-1 table opens in
/// at `grant` over its width (each section slot's region is the split's worker share, driver section.rs; a hash build opens
/// no T1 and keeps its bits); a memo's record count is cut by the same rule. The program's images flag
/// (`node::program_images`, a walk of every pipeline's owner) is read once, at the first share (a memo, not a once site): read per pipeline it bound
/// 4150 partition arms in O(pipelines^2) (crt-0384 / mpart-0002, +30 ms).
struct Bound<'p> {
    program: &'p Program,
    grant: Grant,
    slice: u64,
    images: Cell<Option<bool>>,
}

impl<'p> Bound<'p> {
    fn new(program: &'p Program, grant: Grant, slice: u64) -> Bound<'p> {
        Bound { program, grant, slice, images: Cell::new(None) }
    }

    /// Pipeline `i`'s bits at width `w`: its lowered `bits` re-cut to the slice, or a memo's `rows` records of `rec` bytes.
    fn bits(&self, i: usize, w: u16, bits: u8, rows: Option<(u64, u32)>) -> u8 {
        use gravity_abi::breaker::{granted_bits, machine_bits, partition_bits, t1_default_bytes};
        let bits = match rows {
            Some((r, rec)) => partition_bits(r, rec as u64, self.slice),
            None => machine_bits(bits, self.slice),
        };
        if self.program.pipelines[i].breaker().is_some_and(|b| b.kind == BreakerKind::HashBuild) {
            return bits;
        }
        let images = self.images.get().unwrap_or_else(|| {
            let images = gravity_driver::program_images(self.program);
            self.images.set(Some(images));
            images
        });
        let s = gravity_driver::mem::grant::share_with(self.program, i, self.grant, w, images);
        granted_bits(bits, s.worker.bytes as u64, t1_default_bytes() as u64)
    }
}

impl RunnerPlan {
    pub(super) fn prepare(mcx: Mcx<'static>, a: &mut Admitted, snap: &Rc<SnapshotData<'static>>, width: u16, grant: gravity_driver::Grant, params: &[u64], nulls: &[u8], key: Option<PlanKey>, part_release: PartRelease, bufs: Bufs<'static>) -> PgResult<RunnerPlan> {
        let program = a.program.clone();
        let np = program.pipelines.len();
        let Bufs { mut widths, mut words, mut word_nulls, mut partitions, mut hash_aggs, mut late, spare } = bufs;
        let Spare { mut dense_of, mut rows_of, mut cosplit, mut owners } = spare;
        let mut claims: Vec<Claims> = (0..np).map(|_| Claims::None).collect(); // alloc: Exec
        sized(&mut widths, np, || 0)?;
        let mut dense = None;
        sized(&mut dense_of, np, || None)?;
        // B04: the rows each pipeline's census keeps (the zone predicates' granules), the dense area's folds bound
        sized(&mut rows_of, np, || None)?;
        let mut resolved = Box::new(granule_source::Resolved::new()); // alloc: Exec
        let mut bitmaps: Vec<BitmapRun> = Vec::new(); // alloc: Exec
        let mut leaves: Vec<LeafRun> = Vec::new(); // alloc: Exec
        let mut ranges: Vec<RangeRun> = Vec::new(); // alloc: Exec
        let mut drains = false;
        resolved.set_table_cap(grant.bytes);
        part_release.apply(&mut resolved);
        // D3586: the arms the initial pruning removes, before any claim is sized or source bound (`Initial`)
        let initial = crate::ops::append::proto::Initial::decide(&program, a)?;
        initial.pruned_sites(&program).into_iter().for_each(|s| resolved.prune_site(s));
        // D3663: the relation sets the initial pruning keeps members of, resolved over those members alone
        super::drive::file_pruned_sets(mcx, &initial, &a.registry, &mut resolved)?;
        let hashagg = matches!(program.pipelines[0].ends_in, End::Breaker(Breaker { kind: BreakerKind::HashAgg, .. }));
        // a column's code lane groups by code per part; a path's (M6 R3, `shred_kind::TEXT_CODES`) is only a filter's compare
        let site0 = usize::try_from(program.pipelines[0].driving_site).ok().map(|s| &program.sites[s]);
        // D2998: only a record with a code key groups by code (`fold_codes_by_value` folds those alone); a key computed
        // over a code column (`substring(c_phone, 1, 2)`) is a value, so its pipeline cuts granule claims as any other.
        // Whole-part claims engaged 2 workers over a 2-part customer at W8: 364 ms where granule claims took 60
        let code_keys = program.pipelines[0].owner.as_ref().is_none_or(|o| o.layout.key_kinds().contains(&gravity_abi::breaker::KeyKind::Code));
        let text_groups = hashagg && code_keys && site0.is_some_and(|s| s.lanes().iter().enumerate().any(|(k, l)| l.kind == gravity_abi::ctx::lane::KIND_CODES && s.shred_of(k).is_none()));
        // D2914 (2a): a pipeline that runs only nested, on the leader through its width-1 arm, is bound at the width it runs
        // at, 0: its claims, its group walk's re-check, its breaker bits and its grant share (`crate::ops::nested_members`,
        // read where the statement runs wide: serially every pipeline is bound at the leader's width already)
        let rerun_sections = crate::inputs::knobs::width::rerun_sections_on();
        let nested = if width >= 2 { crate::ops::nested_members(&program, rerun_sections) } else { Vec::new() }; // alloc: Exec
        // D3586: what the loop asks of the protos per pipeline and per site, gathered in one walk of them: a co-split merge
        // join's pipelines, and each site's word-zone slots (`site_zone`, `has_word_zone`). A walk of every proto per
        // pipeline was quadratic in an Append's arms: a pruned generic EXECUTE over 2000 partitions spent 15 of its 29 ms in them
        let protos = program.v4.as_deref().map_or(&[][..], |d| &d.protos[..]);
        sized(&mut cosplit, np, || false)?;
        for q in protos.iter().filter(|q| matches!(q.kind, gravity_program::protos::ProtoKind::CoSplit | gravity_program::protos::ProtoKind::SpoolChunks)) {
            if let Some(c) = cosplit.get_mut(q.pipeline as usize) {
                *c = true;
            }
        }
        // D3336: a window read by claims (`win.partruns`, the Window proto's `WINDOW_PARTED`): its reader (`peer`) claims the
        // window's objects, which the Window protocol's execution answers (ops/window/proto.rs `Parts`); its driving site
        // nominal
        for q in protos.iter().filter(|q| q.kind == gravity_program::protos::ProtoKind::Window && q.arg & gravity_program::protos::WINDOW_PARTED != 0) {
            if let Some(c) = cosplit.get_mut(q.peer as usize) {
                *c = true;
            }
        }
        let zone_slots = crate::ops::scan::pgrc::verdicts::ZoneSlots::new(&program);
        for (p, pipe) in program.pipelines.iter().enumerate() {
            let width = if nested.get(p).copied().unwrap_or(false) {
                if pipe.is_eligible() && pipe.driving_site >= 0 {
                    fires::fire("exec.nested.width0"); // shapecheck fire signal (D2914 (2a)): a nested-only pipeline bound at width 0 under a wide statement
                }
                0
            } else {
                width
            };
            widths[p] = if pipe.is_eligible() { width } else { 0 };
            if pipe.driving_site < 0 {
                continue;
            }
            // D3190: a co-split merge join's claims are its protocol's (the parts of its two sorted runs, exec
            // ops/sort/cosplit.rs); its driving site is the outer producer's, nominal (no row is read through it), so it
            // cuts no claims of its own: a site with no range recipe (a TABLESAMPLE, an index walk) is never asked for one.
            // D3054 / D3256: so is a spool reader's by record claims (`nest.spool.chunks`: a grouping-set arm, a CTE reader):
            // its claims are the sealed spool's records, its driving site its fill's or the CTE plan's, nominal
            if cosplit[p] {
                continue;
            }
            if let gravity_program::ClaimKind::SpillPartitions { slot } = pipe.claims {
                claims[p] = Claims::Partitions(gravity_parallel::PartitionRange::deferred(), slot);
                continue;
            }
            if pipe.claims == gravity_program::ClaimKind::OwnerRuns {
                if widths[p] >= 2 {
                    fires::fire("exec.emit.wide"); // shapecheck fire signal (D3024 (b)): a group emit cut into claims for a section
                    if protos.iter().any(|q| q.kind == gravity_program::protos::ProtoKind::EmitRuns && q.pipeline as usize == p) {
                        fires::fire("exec.emit.runs"); // shapecheck fire signal (D3024 (b)): a group sort's claims sort runs the drain merges
                    }
                }
                claims[p] = Claims::Slices(owner_run_slices(widths[p]));
                continue;
            }
            // D3024 (b): a build drain's claims are the same fixed slices, over the build's record index (the kernel
            // maps a claim to its records and spilled partitions at the call), so every path that runs it drains each
            // record once
            if let gravity_program::ClaimKind::BuildRecords { .. } = pipe.claims {
                claims[p] = Claims::Slices(owner_run_slices(widths[p]));
                drains = true;
                continue;
            }
            let site = pipe.driving_site as usize;
            // A bitmap morsel site's claims are its frozen entries, sized when the pipeline starts (`late_words`
            // builds the bitmap after its runtime keys are bound); the width is the heap rule's over the expanded pages then
            if let Some(r) = a.registry.bitmap_morsel(site) {
                let est = pipe.predicted_rows.min(heap_source::nblocks(mcx, r.range.heap)?);
                claims[p] = Claims::Blocks(BlockRange::new(0, ClaimSize::Adaptive { startup_c0: startup_c0(est, width, BLOCK_C0, est.max(BLOCK_C0)) }));
                bitmaps.push(BitmapRun { pipeline: p, recipe: r.clone(), slots: program.sites[site].param_slots.to_vec(), granted: width, width: Cell::new(None), leaves: if pipe.is_eligible() { leaf_width(pipe.predicted_rows, width) } else { 0 } }); // alloc: Exec
                continue;
            }
            // An index-only site the pool reads by leaf pages: every claim joins the run's shared walk (a claim after its
            // end gets no row), so one per participant and one spare; the walk is captured when a run starts
            // (`late_words`). Under two leaf workers, without the recipe's leaf form, or in an instrumented execution
            // (EXPLAIN ANALYZE: the serial walk's Heap Fetches are PostgreSQL's), the leader walks it alone.
            // An ordered index site the pool reads in key ranges: a claim is a range in index order (the emit hands the
            // claims' rows on in claim order), cut when a run starts (`late_words`); one claim until then. Under two
            // workers (the leaf walk's rule), without the recipe's ranges form, or in an instrumented execution, the
            // leader walks it alone, one claim.
            if program.sites[site].ranges() {
                let w = if pipe.is_eligible() && !crate::admit::is_instrumented() { leaf_width(pipe.predicted_rows, width) } else { 0 };
                // the participants' heap reads publish attribute lanes and a last RowId lane (hosts index_probe.rs)
                let attrs = gravity_hosts::ops::scan::index_probe::lanes_served(&program.sites[site]);
                match a.registry.range_site(site).filter(|_| w >= 2 && attrs) {
                    Some(r) => {
                        let cell = Arc::new(gravity_hosts::ops::scan::index_ranges::RangeCell::default()); // alloc: Exec
                        resolved.file_range_cell(site, cell.clone());
                        claims[p] = Claims::Blocks(BlockRange::new(1, ClaimSize::Fixed(1)));
                        ranges.push(RangeRun { pipeline: p, recipe: r.clone(), slots: program.sites[site].param_slots.to_vec(), granted: w, cell, width: Cell::new(None) }); // alloc: Exec
                    }
                    None => {
                        widths[p] = 0;
                        if pipe.is_eligible() {
                            claims[p] = Claims::Blocks(BlockRange::new(1, ClaimSize::Fixed(1)));
                        }
                    }
                }
                continue;
            }
            if program.sites[site].leaves() {
                let w = if pipe.is_eligible() && !crate::admit::is_instrumented() { leaf_width(pipe.predicted_rows, width) } else { 0 };
                match a.registry.leaf_site(site).filter(|_| w >= 2) {
                    Some(r) => {
                        let cell = Arc::new(gravity_hosts::ops::scan::index_leaves::LeafCell::default()); // alloc: Exec
                        resolved.file_leaf_cell(site, cell.clone());
                        claims[p] = Claims::Blocks(BlockRange::new(w as u64 + 1, ClaimSize::Fixed(1)));
                        leaves.push(LeafRun { pipeline: p, recipe: r.clone(), slots: program.sites[site].param_slots.to_vec(), granted: w, cell, width: Cell::new(None) }); // alloc: Exec
                    }
                    None => {
                        // the leader's serial walk of the whole index: one claim (the width-1 arm of an eligible pipeline
                        // runs every claim, and each would walk it whole again)
                        widths[p] = 0;
                        if pipe.is_eligible() {
                            claims[p] = Claims::Blocks(BlockRange::new(1, ClaimSize::Fixed(1)));
                        }
                    }
                }
                continue;
            }
            // a serial pipeline cuts no claims unless its site is a range or granule source (the claim cutters'): an index
            // walk's, or a rung's own source (TID, TABLESAMPLE, the bitmap tree: M2, D394) walks its relation itself
            if !pipe.is_eligible() && serial_host_site(&a.registry, site) {
                continue;
            }
            let wants_dense = pipe.words.is_some_and(|d| d.dense_lo != WordsDesc::NONE || d.dense_hi != WordsDesc::NONE);
            // NEW-72: the site's zone predicates as this execution binds them (a bound literal's value, an IN list's members)
            let zone = zone_slots.site_zone(&program, site, params, nulls);
            // sortmerge: a merged site reads its parts in key order itself (hosts `MergedRangeSource`): no zone walk
            let walk = a.walk.or(declared_walk(pipe).filter(|w| w.attno > 0)).filter(|_| !program.sites[site].is_merged());
            let (c, d, w, r) = claims_of(mcx, &a.registry, site, &program.sites[site], zone.as_deref(), snap, width, walk, (wants_dense || p == 0 && matches!(pipe.ends_in, End::Breaker(Breaker { kind: BreakerKind::HashAgg, .. }))).then(|| pipe.owner.map_or(0, |o| o.dense_attno)), p == 0 && text_groups, has_word_zone(&zone_slots, site), resolved.site_pruned(site), &mut resolved)?;
            claims[p] = c;
            dense_of[p] = d;
            rows_of[p] = r;
            if pipe.is_eligible() {
                widths[p] = w;
            }
            if p == 0 {
                dense = d;
            }
        }
        // A bitmap morsel site no pipeline drives (a nest loop's or a nested SubPlan's embedded inner, run per
        // outer row) is built here, once: its keys read no executor parameter (decompose `morsel_form`), so they are bound
        // at the bind (no pipeline owns them as late words) and every rescan walks the same frozen entries
        for (site, d) in program.sites.iter().enumerate() {
            let Some(r) = a.registry.bitmap_morsel(site) else { continue };
            if program.pipelines.iter().any(|q| q.driving_site >= 0 && q.driving_site as usize == site) {
                continue;
            }
            let (keys, knulls): (Vec<u64>, Vec<u8>) = d.param_slots.iter().map(|&s| if s >= 0 { (params.get(s as usize).copied().unwrap_or(0), nulls.get(s as usize).copied().unwrap_or(1)) } else { (0, 0) }).unzip(); // alloc: Exec
            r.build(snap.clone(), &keys, &knulls, 0)?;
            bitmaps.push(BitmapRun { pipeline: usize::MAX, recipe: r.clone(), slots: Vec::new(), granted: 0, width: Cell::new(Some(0)), leaves: 0 }); // alloc: Exec
        }
        let mut filed = 0u32;
        // D3014 phase 3: under a width of two no pipeline runs a section (gravity_parallel `Section::plan`), so no worker
        // probes a site: the leader's own index source serves it, as P3's, and no image is captured
        if width >= 2 && !capture_probes(mcx, a, &mut resolved, &mut filed)? {
            widths.iter_mut().for_each(|w| *w = 0);
            fires::fire("nest.probe.serial");
        } else if filed > 0 {
            fires::fire("nest.probe.image");
        }
        // D1322: a granule site no pipeline drives (a nest.loop side's scan) binds against the same Resolved
        let _undriven = super::drive::install_undriven(mcx, &a.registry, snap, &mut resolved)?;
        let order_groups = a.nsort > 0 && hashagg && !text_groups;
        // the statement's words, then the kernels' declared facts and the v4 protocols' words (`run_words`)
        let nk = params.len();
        let nw = run_words(&program, nk);
        sized(&mut words, nw, || 0)?;
        sized(&mut word_nulls, nw, || 0)?;
        words[..nk].copy_from_slice(params);
        word_nulls[..nk].copy_from_slice(&nulls[..nk]);
        let mut nparams = nk;
        // the declared descriptors (milestone-2.md §2.6 items 12–13, 16, 18): the only binding (no hand kernel's by-identity arm remains)
        let two = program.pipelines.iter().enumerate().find_map(|(i, p)| p.admit.map(|d| (i, d)));
        let desc = program.pipelines.iter().find_map(|p| p.owner);
        let group_walk = a.walk.filter(|w| w.groups);
        // DR-6: each partitioned breaker's bits bound at run time (`Bound`), at this machine's slice: a quarter of its L2
        // share (the reference's 512 KiB on c8g, where the lowered bits stand)
        let slice = match gravity_driver::sink::partition_bytes() / 4 {
            0 => gravity_abi::breaker::PARTITION_SLICE,
            s => s,
        };
        let bound = Bound::new(&program, grant, slice);
        for (i, p) in program.pipelines.iter().enumerate() {
            let Some(d) = p.words else { continue };
            let bits = match p.breaker().map(|b| b.spill) {
                Some(gravity_abi::breaker::Spill::Partitioned { bits }) => bound.bits(i, widths[i], bits, None) as u64,
                _ => 0,
            };
            let (lo, hi) = dense_of[i].unwrap_or((1, 0));
            let facts = [group_walk.map_or(0, |w| w.n as u64), lo as u64, hi as u64, bits, (order_groups || group_walk.is_some()) as u64];
            for (slot, fact) in d.slots().into_iter().zip(facts).filter(|&(s, _)| s != WordsDesc::NONE) {
                // a graph's bound words carry their kinds: a declared fact lands on a reserved (Zero) word, never on a literal, an extern or a key expression
                assert!(a.graph.as_ref().map_or(slot as usize >= nk, |g| !matches!(g.words.get(slot as usize), Some(admit::GraphWord::Const { .. } | admit::GraphWord::Extern(_) | admit::GraphWord::Expr(_) | admit::GraphWord::Late { .. } | admit::GraphWord::Deferred))), "gravity: a kernel word over a key param");
                words[slot as usize] = fact;
            }
            nparams = nparams.max(d.nparams as usize);
        }
        let v4 = crate::ops::State::prepare(&mut crate::ops::Prepare { mcx, program: &program, admitted: &*a, snap, width, grant, widths: &mut widths, words: &mut words, nulls: &mut word_nulls, nparams: &mut nparams, initial: Some(&initial), owner: gravity_driver::stmt_context().map(|(o, _)| (o, memledger::Kind::Region)) })?;
        let hash_build = HashBuildPartial;
        let walk = a.walk.or_else(|| program.pipelines.iter().find_map(declared_walk));
        let topk_lanes = if a.walk.is_some() { None } else { program.pipelines.iter().find_map(|p| p.topk).map(|d| d.code_lanes) };
        let two_pass = two.is_some();
        let (pass1, pass2_idx) = two.map_or((0usize, 1usize), |(i, d)| (d.pass1 as usize, i));
        let memo = key.and_then(crate::admit::memo::groups_for).filter(|_| !two_pass && desc.is_some());
        // each declaring pipeline is bound over its own breaker's bits (the exporting one over its words block), with (D597) a
        // dense-bound owner's form without its domain, the round-1 rerun's after a key outside it; D3586: by pipeline, the
        // declaring ones only (a per-pipeline table copied the owner functions for each of a pruned Append's arms)
        owners.clear();
        room(&mut owners, np)?;
        for (i, p) in program.pipelines.iter().enumerate() {
            let Some(d) = p.owner else { continue };
            let bits = match (memo, p.breaker().map(|b| b.spill)) {
                (Some(m), _) => bound.bits(i, widths[i], 0, Some((m.0, d.layout.size()))),
                (None, Some(gravity_abi::breaker::Spill::Partitioned { bits })) => bound.bits(i, widths[i], bits, None),
                _ => 0,
            };
            let base = two.filter(|(_, t)| t.pass1 as usize == i).map_or(0, |(_, t)| t.base as usize);
            // the declared words are absolute slots: the dense domain is read only over the whole word block
            let (f, g) = crate::table::owner::bind_dense_plain_at(&d, &mut words[base..], bits, None, p.words.filter(|_| base == 0), p.is_eligible(), rows_of[i]);
            owners.push((i, f, g));
        }
        let owner_of = |i: usize| owners.binary_search_by_key(&i, |o| o.0).ok().map(|k| (owners[k].1, owners[k].2));
        // D2914: each HashAgg merge bounds by its own pipeline's group walk (a program may hold one per InitPlan): the first walk's
        // n bounding every merge stopped a larger walk short of its groups
        let limit = |i: usize| a.walk.or_else(|| declared_walk(&program.pipelines[i])).filter(|w| w.groups).map_or(0, |w| w.n);
        let owner_at = two_pass.then_some(pass2_idx).filter(|&q| owner_of(q).is_some()).or_else(|| owners.first().map(|o| o.0));
        let program_owner = owner_at.and_then(owner_of).map(|o| o.0);
        let program_plain = owner_at.and_then(owner_of).and_then(|o| o.1);
        // GP-1b: a descending group walk's bound is over the NOT of the key (the walk's NOT-of-maximum order)
        let down = |i: usize| a.walk.or_else(|| declared_walk(&program.pipelines[i])).is_some_and(|w| w.groups && w.desc);
        let hashed = |i: usize| matches!(program.pipelines[i].ends_in, End::Breaker(Breaker { kind: BreakerKind::HashAgg | BreakerKind::Distinct, .. }));
        hash_aggs.clear();
        room(&mut hash_aggs, np)?;
        for i in (0..np).filter(|&i| hashed(i)) {
            let (declared, plain) = owner_of(i).map_or((None, None), |(f, g)| (Some(f), g));
            let h = if two_pass && i == pass1 { HashAggPartial::with_plain(0, declared, plain) } else if declared.is_some() { HashAggPartial::with_plain(limit(i), declared, plain).descending(down(i)) } else { HashAggPartial::with_plain(limit(i), program_owner, program_plain).descending(down(i)) };
            hash_aggs.push((i, h));
        }
        // lane setops: a TopK breaker outside a zone walk (the Append's children) names its `n` by its bytes
        let topk_n = a.walk.or_else(|| program.pipelines.iter().filter_map(declared_walk).find(|w| !w.groups)).map(|w| w.n).or_else(|| program.pipelines.iter().find_map(|p| match p.ends_in {
            End::Breaker(Breaker { kind: BreakerKind::TopK, bytes, .. }) => Some(bytes.saturating_sub(core::mem::size_of::<gravity_abi::breaker::TopK>() as u32) / (program.layout.out_natts.max(1) as u32 * 9)),
            _ => None,
        })).unwrap_or(0);
        let top_k = TopKPartial { n: topk_n, natts: program.layout.out_natts as u32 };
        let pass2_site = if two_pass { usize::try_from(program.pipelines[pass2_idx].driving_site).unwrap_or(usize::MAX) } else { usize::MAX };
        let pass2_rel = a.registry.granule_recipe(pass2_site).map_or(0, |r| r.rel);
        // the relations the sites read, and every relation of the plan's range table (an init plan's by-reference param may
        // point into one): the relations PG's executor locks. Not `relationOids`, PG's invalidation list, which also names a
        // relation only a regclass constant mentions: PG never locks it, and resolve's AccessShareLock waited behind another
        // session's AccessExclusiveLock (D759). Only a pool claim reads the map, and a claim runs only in a section, which
        // `Section::plan` opens for an eligible pipeline with a driving site at a planned width of 2 or more: a run whose
        // widths plan no section (a many-arm Append of small partitions, each serial under by_work's heap rule) resolves
        // none (a count over 300 small partitions at W8 opened 300 toast relations for no claim: 6 ms of its 24)
        let sections = (0..np).any(|p| gravity_parallel::Section::plan(&program, p, widths[p], true).is_some());
        let toast = if width >= 2 && sections {
            let plan_rels = a.pstmt.into_iter().flat_map(|p| p.rtable.iter()).filter_map(|n| n.as_range_tbl_entry()).filter(|r| r.rtekind == types_nodes::parsenodes::RTEKind::RTE_RELATION).map(|r| r.relid);
            Some(Arc::new(heaptoast::worker::resolve(mcx, a.registry.relids().chain(plan_rels))?)) // alloc: Exec
        } else {
            None
        };
        // D899: the late key words wait for their pipelines (`admit::GraphWord::Late`; bound at bind when `late_ok` is not)
        late.clear();
        if let Some(g) = a.graph.as_ref().filter(|g| g.late_ok()) {
            room(&mut late, g.words.len())?;
            for (w, x) in g.words.iter().enumerate() {
                if let admit::GraphWord::Late { expr, pipeline } = *x {
                    late.push((pipeline as usize, w, expr));
                }
            }
        }
        let late_exprs: *mut crate::shapes::KeyExprs = &mut a.key_exprs;
        partitions.clear();
        room(&mut partitions, np)?;
        for (p, c) in claims.iter().enumerate() {
            if let Claims::Partitions(_, slot) = c {
                partitions.push((p, *slot));
            }
        }
        let sorted_emits = program.v4.as_deref().is_some_and(|d| d.protos.iter().any(|q| q.kind == gravity_program::protos::ProtoKind::EmitRuns));
        let spare = Spare { dense_of, rows_of, cosplit, owners };
        Ok(RunnerPlan { bitmaps, leaves, ranges, snap: snap.clone(), pulled: Default::default(), late, late_exprs, late_mcx: mcx, v4, toast, program, widths, claims, partitions, sorted_emits, emit_leader: Default::default(), drains, drain_leader: Default::default(), text_groups, hash_build, hash_aggs, two_pass, pass1, pass2_idx, two: two.map(|(_, d)| d), pass2: pgsync::OnceLock::new(), pass2_rel, pass2_site, admitted: Cell::new((0, 0)), exported: Cell::new(0), top_k, walk, topk_lanes, sections: Cell::new(0), params: words, nulls: word_nulls, nparams, dense, resolved, key: key.filter(|_| !two_pass && desc.is_some()), rerun_sections, spare })
    }

    /// The plan's vectors back to `s` for the level's next plan, the rest of it dropped.
    pub(super) fn give_back(self, s: &mut PlanScratch) {
        let RunnerPlan { widths, partitions, hash_aggs, params, nulls, late, spare, .. } = self;
        s.give(Bufs { widths, words: params, word_nulls: nulls, partitions, hash_aggs, late, spare });
    }
}

/// The steps after producer `pipeline` ran that every producer takes, before the v4 protocols (D262: a protocol that acted
/// skipped them, so a spilled build it read met no D133 wall and its partition phase no claims): D133's wall for a spilled
/// hash build no pipeline reads (it would probe an emptied table), then `set_total(slot, parts)` for the pipelines over
/// the spill partitions of the slot it produced; then `v4`. `Some` = done: an error, or a protocol acted (the shape
/// table's own steps are skipped); `None` = the shape table's steps run.
pub fn producer_ran<E>(pipes: &[gravity_program::Pipeline], pipeline: usize, exec: &mut E, parts_of: impl Fn(&E, u8) -> u32, set_total: &mut dyn FnMut(u8, u64), v4: impl FnOnce(&mut E) -> Option<gravity_driver::Result<()>>) -> Option<gravity_driver::Result<()>> {
    if let Some(Breaker { kind: BreakerKind::HashBuild, slot, .. }) = pipes[pipeline].breaker() {
        let read = pipes.iter().any(|q| matches!(q.claims, gravity_program::ClaimKind::SpillPartitions { slot: s } if s == slot || q.consumes.contains(&slot)));
        if !read && parts_of(exec, slot) > 0 {
            return Some(Err(gravity_driver::Error::host(SPILLED_UNREAD)));
        }
    }
    if let Some(slot) = pipes[pipeline].produces() {
        set_total(slot, parts_of(exec, slot) as u64);
    }
    v4(exec)
}

/// The words a run of `program` binds over its `nk` key words: those, every v4 protocol's words (`Proto::words`) and the
/// slots the kernels' declared facts land on (`WordsDesc`, a two-pass admit's block, an owner's bits word). Sized by the
/// statement: `limits::PARAM_WORDS` is the lowering's named refusal, never a binding's size.
pub(super) fn run_words(program: &Program, nk: usize) -> usize {
    let protos = program.v4.as_deref().map_or(0, |d| d.protos.iter().map(|p| p.words.base as usize + p.words.n as usize).max().unwrap_or(0));
    let facts = |d: WordsDesc| d.slots().into_iter().filter(|&s| s != WordsDesc::NONE).map(|s| s as usize + 1).fold(d.nparams as usize, usize::max);
    let base = program.pipelines.iter().filter_map(|p| p.admit).map(|t| t.base as usize + t.words as usize).max().unwrap_or(0);
    let kernel = program.pipelines.iter().map(|p| p.words.map_or(0, facts).max(p.owner.and_then(|o| o.bits).map_or(0, |(w, _)| w as usize + 1))).max().unwrap_or(0);
    nk.max(protos).max(base + kernel)
}

/// D3024 (b): a group emit's claims at width `w`: about two per thread in a section (each a fresh gather of its runs), one
/// claim of every slice below width 2 (the serial emit's one gather: a sort end at width 1 seals its run per call).
fn owner_run_slices(w: u16) -> gravity_parallel::PartitionRange {
    use gravity_abi::breaker::OWNER_RUN_SLICES;
    let per = if w < 2 { OWNER_RUN_SLICES } else { OWNER_RUN_SLICES.div_ceil(2 * w as u64) };
    gravity_parallel::PartitionRange::new(OWNER_RUN_SLICES, per)
}

/// D3024 (b), sorted: the owner rule C8 read at run time (no constant of its own). A sorted wide emit (`q`, its
/// `ProtoKind::EmitRuns`: its producer pipeline `peer`, its record's bytes `arg`) whose producer's groups fit half the
/// core's L1d (`gravity_parallel::leader_owns_bytes`, the owner phase's leader fold) runs on the leader: a pool round costs
/// more than the sort. `false` while the producer's slot holds no finished owner region.
fn emit_on_leader(exec: &Exec, program: &Program, q: &gravity_program::protos::Proto) -> bool {
    let Some(slot) = program.pipelines.get(q.peer as usize).and_then(|p| p.produces()) else { return false };
    gravity_abi::breaker::Owned::counts(exec.node().slot_region(slot)).is_some_and(|(records, groups)| groups.unwrap_or(records).saturating_mul(q.arg) <= gravity_parallel::leader_owns_bytes())
}

/// D3024 (b), the owner rule C8 read at run time (no constant of its own): a drain whose build in `slot` spilled
/// nothing and whose records' bytes fit half the core's L1d (`gravity_parallel::leader_owns_bytes`, the owner phase's
/// leader fold) runs on the leader: a pool round costs more than the walk. Only while the records its end's owners fold
/// (`end`'s phase-1 table and stream files, `Node::owner_held`) fit too: the width-1 arm folds them on the leader alone,
/// where the run's sections scattered more than that (D3140) a section's end folds them with the pool's owners. `None`
/// while the slot holds no laid-out build.
fn drain_on_leader(exec: &Exec, slot: u8, end: Option<u8>) -> Option<bool> {
    let bytes = gravity_lowerv4::ops::hashjoin::drain::source::drain_bytes(exec.node().slot_region(slot))?;
    let held = end.map_or(0, |s| exec.node().owner_held(s));
    let leader = gravity_parallel::leader_owns_bytes();
    Some(exec.node().spill_parts_live(slot) == 0 && bytes <= leader && held <= leader)
}

/// M7-SPILL: `c` is a pipeline's spill partitions and its producer wrote none (`after_pipeline` sized them 0).
fn no_partition(c: &Claims) -> bool { matches!(c, Claims::Partitions(r, _) if r.total() == 0) }

/// The top-k run a pipeline declares (`gravity_program::TopKDesc`, lower.md §15.12) as the walk the shape table named:
/// rows counted, the declaring pipeline's claims ordered when `walk_attno > 0`; a hash aggregate's is the group walk (`groups`: the table's cut at n).
fn declared_walk(pipe: &gravity_program::Pipeline) -> Option<Walk> {
    pipe.topk.map(|d| Walk { attno: d.walk_attno, n: d.n, groups: matches!(pipe.ends_in, End::Breaker(Breaker { kind: BreakerKind::HashAgg, .. })), codes: d.codes, secondary: d.secondary, desc: d.walk_desc, nulls_first: d.walk_nulls_first })
}

/// Site `site`'s recipe walks its source itself on a serial pipeline: any recipe but a range or granule one (D394).
fn serial_host_site(registry: &HostRegistry, site: usize) -> bool {
    site < registry.len() && registry.range_recipe(site).is_none() && registry.granule_recipe(site).is_none()
}

/// Site `site` has a word-valued zone predicate (a generic plan's `lane op $n`, `src.verdicts` slots `VERDICT_WORD_ZONE..`):
/// bound after the census, so the catalog's records over the static predicates do not say which parts it skips.
/// NEW-72's slots (a bound literal, an IN list) are not such: `site_zone` binds them before the census.
fn has_word_zone(slots: &crate::ops::scan::pgrc::verdicts::ZoneSlots<'_>, site: usize) -> bool {
    use gravity_program::protos::{word_zone_at_of, word_zone_list_of};
    slots.of(site).any(|q| word_zone_at_of(q.arg).is_none() && word_zone_list_of(q.arg).is_none())
}

/// RUNS-02: whether a group walk over a zone order of minima `mins` (ascending) for `n` distinct first keys costs at least
/// the plain claims at Gravity width `granted`: its reach (`ops::sort::walk::walk_width`) over `granules`, keeping `n`
/// first keys, at the walk's share of the plain chain (`cost::walk::Walk::group_share`, the planner's price, so a census
/// that moved since planning re-decides as the planner would), the key's `spread` read off the zones as the planner
/// reads it (`zone_spread`). The groups it keeps read as n and a key's presence in a granule as none (the run time has
/// no estimate of either), so the re-check never degrades a walk the planner's own estimate keeps.
fn group_walk_dearer(mins: &[i64], n: u32, granules: u64, granted: u16, spread: f64) -> bool {
    let reach = crate::ops::sort::walk::walk_width(mins, n as usize) as u64;
    let profile = gravity_program::cost::walk::Profile::parse(gravity_hosts::inputs::custom_guc_text("pgrust.gravity_cost_profile").as_deref());
    gravity_program::cost::walk::Walk::of(profile).group_share(reach, granules, u64::from(n), u64::from(n), spread, 0.0, u32::from(granted)) >= 1.0
}

/// PL-05: the key's spread over its keyed granules (`cost::walk::key_spread`) from the zone orders the walk reads: the
/// ascending minima `mins` and a descending walk's keys `desc` (each the NOT of a granule's maximum); a keyless granule
/// (`i64::MIN` in either) left out. The planner reads the same from the zones' (min, max) pairs.
fn zone_spread(mins: &[i64], desc: &[i64]) -> f64 {
    fn keyed(v: &[i64]) -> impl Iterator<Item = i64> + '_ {
        v.iter().copied().filter(|&k| k != i64::MIN)
    }
    let (lo, n) = (keyed(mins).min().unwrap_or(0), keyed(mins).count() as u64);
    let hi = keyed(desc).min().map_or(lo, |k| !k);
    let span = keyed(desc).map(|k| (!k) as f64).sum::<f64>() - keyed(mins).map(|k| k as f64).sum::<f64>();
    gravity_program::cost::walk::key_spread(span, n, lo, hi)
}

#[allow(clippy::too_many_arguments)]
/// `zone`: the site's zone predicates as this execution binds them (`verdicts::site_zone`; `None`: it takes none).
/// `pruned`: no pipeline this execution runs reads the site (`Initial::pruned_sites`): a heap relation is sized as one
/// of no blocks, not opened (D3586), as its source binds (hosts heap_source.rs `bind_pruned`).
fn claims_of(mcx: Mcx<'static>, registry: &HostRegistry, site: usize, desc: &gravity_program::SiteDesc, zone: Option<&[gravity_program::ZonePred]>, snap: &Rc<SnapshotData<'static>>, granted: u16, walk: Option<Walk>, group_key: Option<i16>, code_groups: bool, word_zone: bool, pruned: bool, resolved: &mut granule_source::Resolved) -> PgResult<(Claims, Option<(i64, i64)>, u16, Option<u64>)> {
    if let Some(r) = registry.range_recipe(site) {
        if walk.is_some() {
            return Err(glue::internal("gravity: a zone walk over a heap relation"));
        }
        // D3586: an arm the initial pruning removed hands out no claim, nor (D3663) a relation set it removed whole
        // D3585: a relation set's claims tile its relations' blocks, one relation per claim; the sources take the
        // relations from the execution's `Resolved`, where (D3663) a pruned set's survivors are filed already
        let set = match r.set.is_some() && !pruned {
            true => Some(match resolved.rel_set(site) {
                Some(s) => s.clone(),
                None => Arc::new(heap_source::RelSet::resolve(mcx, r)?), // alloc: Exec
            }),
            false => None,
        };
        let blocks = match &set {
            Some(s) => s.total(),
            None if pruned => 0,
            None => heap_source::nblocks(mcx, r.heap)?,
        };
        // a relation of no blocks hands out no claim: the serial arm's section-end call is what opens the leader's table;
        // any other runs `by_work`'s heap rule (one worker per BLOCK_C0 blocks up to the grant, the leader alone under
        // two), as a pipeline that declares a Width proto already does, so a small partition of a many-arm Append wakes no
        // worker it has no claim for; the claims tile the relation either way
        let width = match blocks {
            0 => 1,
            _ if granted < 2 || forced_width().is_some() => granted,
            _ => gravity_program::heap_width(blocks, granted),
        };
        let size = ClaimSize::Adaptive { startup_c0: startup_c0(blocks, granted, BLOCK_C0, blocks) };
        if let Some(s) = set {
            let cursor = gravity_parallel::RangeCursor::with_boundaries(blocks, size, s.bounds());
            resolved.file_rel_set(site, s);
            return Ok((Claims::Blocks(BlockRange(cursor)), None, width, None));
        }
        return Ok((Claims::Blocks(BlockRange::new(blocks, size)), None, width, None));
    }
    if let Some(r) = registry.granule_recipe(site) {
        let rel = table::table_open(mcx, r.rel, types_storage::lock::NoLock)?;
        let list = granule_source::resolve(&rel, Some(snap));
        // the census from the resident zone columns (the group key's extrema: the dense front's bound, or the owner's
        // `t3_dense::word` for a key it names; the granules the zone predicates keep): no part walked, no record decoded
        let census = list.as_ref().ok().map(|list| -> PgResult<_> {
            let dense = if let Some(a) = group_key.filter(|_| desc.natts() > 0 && dense_on()) { granule_source::zone_extrema_of(&rel, list, if a > 0 { a } else { desc.out_attnos[0] } as u32)?.filter(|&(lo, hi)| if a > 0 { gravity_kernels::ops::sort::topn::t3_dense::word(lo, hi) != 0 } else { gravity_abi::breaker::DenseFront::domain(lo, hi).is_some_and(|n| gravity_abi::breaker::DenseFront::acc_fits(n, gravity_driver::machine::profile().partition_bytes)) }) } else { None };
            Ok((granule_source::zone_census(&rel, list, &r.attnos[..r.natts], zone.unwrap_or(&[]))?, dense))
        });
        // the walk's order from the resident zone store (no stats record read per granule, no sort per statement); a
        // NULLS FIRST walk over a nullable key from the granules' stats records (their non-null counts), resident too
        let zone_order = match (walk.filter(|w| w.attno > 0), list.as_ref().ok()) {
            (Some(w), Some(list)) if w.nulls_first => Some(granule_source::nulls_first_walk_order(&rel, list, w.attno as u32, w.desc)?),
            (Some(w), Some(list)) => granule_source::zone_walk_order(&rel, list, w.attno as u32, w.desc)?,
            _ => None,
        };
        // a group walk's re-check reads the key's spread off the descending order beside it (resident, as the other)
        // (GP-1b: a descending group walk's own order is the descending one, the ascending read beside it)
        let group_spread = match (walk.filter(|w| w.attno > 0 && w.groups && !w.nulls_first), list.as_ref().ok(), zone_order.as_ref()) {
            (Some(w), Some(list), Some(own)) if !crate::inputs::knobs::width::walk_recheck_off() => granule_source::zone_walk_order(&rel, list, w.attno as u32, !w.desc)?.map(|o| if w.desc { zone_spread(&o.mins, &own.mins) } else { zone_spread(&own.mins, &o.mins) }),
            _ => None,
        };
        rel.close(types_storage::lock::NoLock)?;
        let list = list?;
        let ((surviving, dead), dense) = census.expect("a resolved list")?;
        let rows = Some(list.rows().min(surviving.saturating_mul(granule_source::ROWS as u64)));
        // a sparse survivor set (fewer live granules than one standard claim per worker) is cut one
        // granule per claim over every granted worker and, without per-part code groups (`code_groups`:
        // the text-groups fold appends per part), never runs whole parts: 91 live granules in 4 parts
        // engaged 4 workers for 3.2 ms as 4 whole-part claims (the lab: 0.5 ms on 16)
        let sparse = surviving < GRANULE_C0 * granted as u64 && forced_c0().is_none() && forced_width().is_none();
        let live = |p: usize| !dead.iter().any(|&(lo, hi)| lo <= list.starts()[p] && list.end_of(p) <= hi);
        let live_parts = (0..list.len()).filter(|&p| live(p)).count() as u64;
        let width = if sparse {
            // whole-part claims engage one worker per live part (a two-pass form over one part stays serial)
            (if code_groups { live_parts } else { surviving }).clamp(1, granted as u64) as u16
        } else {
            width_for(surviving, granted)
        };
        // an answerable site's whole parts buy its part-grain skips and answers: only when the catalog's records skip or
        // answer some part with no read (or cannot say); with every part Mixed they leave `width - live_parts` workers
        // idle (a hashed int4 BETWEEN at W8, every granule Mixed: 0.906 ms whole, 0.436 cut), and with more parts than
        // workers they keep the last wave whole (no tail rule, no end-game split: a code lane's text `<> c` over 20 Mixed
        // parts at W8, 2.7 -> 3.4 ms whole). Asked where the law turns on it.
        let zone = zone.unwrap_or(&[]);
        let lanes_answer = !zone.is_empty() && (0..desc.natts()).all(|k| desc.filter_lanes.contains(k) || desc.answer_lanes.contains(k));
        let answerable = lanes_answer
            && (sparse || word_zone || {
                let answer: Vec<usize> = (0..desc.natts()).filter(|&k| desc.answer_lanes.contains(k)).collect(); // alloc: Exec
                granule_source::answered_parts(&list, r, zone, &answer).is_none_or(|n| n > 0)
            });
        // D4324 step 3, R3 (LADDER-DESIGN): while the ladder is short past its caches, claims split inside parts
        let whole = whole_part_claims(code_groups, answerable, sparse, live_parts, width) && forced_c0().is_none() && !mcx::ladder::narrowed();
        let cut = lanes_answer && !answerable && !code_groups && forced_c0().is_none();
        let c0 = if sparse { 1 } else { forced_c0().unwrap_or_else(|| startup_c0(list.granules(), width, GRANULE_C0, list.max_part_granules())) };
        if let Some(w) = walk.filter(|w| w.attno > 0) {
            // the zone walk (§1.10 increment 3 step 5): the granules in key-minimum order (descending: key-maximum), a
            // claim of `c0` granules inside one part; the source maps the claim's units through
            // the same order; the section stops at the bound the workers' partials prove
            let zo = match zone_order {
                Some(o) => o,
                None => Arc::new(granule_source::ZoneOrder::of(&granule_source::granule_walk_keys(&list, w.attno as u32, w.desc, w.nulls_first)?, &list)?), // alloc: Exec
            };
            // PL-06's run-time re-check: a NULLS FIRST walk whose every granule holds a NULL (every key the lowest, never
            // past a bound) skips nothing, so the plain claims below read the same granules without one-granule claims
            // RUNS-02's: a group walk whose reach (the granules to the n-th distinct exact minimum) the walk's price
            // (cost_profile walk.rs, at the statement's width) puts at or past the plain read takes the plain claims; its
            // cut and keyed skip hold in any claim order (each worker's bound is at least the statement's n-th key)
            let plain = w.groups && !crate::inputs::knobs::width::walk_recheck_off() && group_walk_dearer(&zo.mins, w.n, list.granules(), granted, group_spread.unwrap_or(0.0));
            if plain {
                fires::fire("walk.group.plain"); // shapecheck fire signal (RUNS-02: the run-time census sent a group walk to the plain claims)
            }
            if !plain && !(w.nulls_first && zo.mins.last().is_none_or(|&m| m == i64::MIN)) {
                // one granule per first claim: the bound is known after the first window, and a
                // walk that started with `c0` granules per worker would touch W × c0 before it
                let cursor = GranuleRange::ordered(&zo.order, &zo.mins, |g| zo.part_of[g as usize] as usize, ClaimSize::Adaptive { startup_c0: 1 });
                resolved.install(r.rel, list);
                resolved.set_order(site, Some(Arc::clone(&zo.order)));
                return Ok((Claims::Granules(cursor), dense, width, rows));
            }
        }
        let mut cursor = GranuleRange::over_parts(list.granules(), ClaimSize::Adaptive { startup_c0: c0 }, list.starts());
        if cut {
            fires::fire("pgrc.claims_cut");
        }
        // every claim runs its whole part: one run read per lane per part and W parts' reads in flight
        // (prior-art-cold.md's census: sizer-cut claims cold put 16 workers inside 1–2 parts at depth 2.8)
        cursor.set_whole_boundary_claims(whole);
        // the tail rule: the last P mod W live parts (cursor order) cut inside each part by the sizer, the rest whole; the
        // runtime's end-game split stays off them, so the whole waves stay whole at any width. Not where a part may go to
        // the owner as its dictionary's entry runs (road dict.entries: a plain count(DISTINCT) of one code lane, the
        // source's `runs_ok`): that is part-grain work like an answer, and every source that touches a cut part publishes
        // the part's whole runs again (v4/m1_runs_spill's 12 parts at W8: recs 600,000 -> 650,000, one source more)
        let entry_runs = desc.entries == Some(0) && desc.natts() == 1 && !desc.entries_filtered && !desc.entries_nulls;
        // D4732 (a): nor where a part it would cut ranks a verbatim column at its first open (`row_codes`: the whole column
        // decoded and every row sorted by its bytes, part-grain work no cut shares): a cut part's claims go to every worker
        // in turn, so its one build runs while the others wait on it, part after part (7 cut parts of a verbatim text
        // column at W8 ranked one at a time: the section's end 1.9 -> 5.9 s). Whole, such parts rank side by side, and a
        // part whose table is resident (a hot read) still cuts
        let ranks = |h: u64| -> PgResult<bool> {
            let rel = table::table_open(mcx, r.rel, types_storage::lock::NoLock)?;
            let lanes = granule_source::verbatim_code_lanes(&rel, r);
            rel.close(types_storage::lock::NoLock)?;
            let lanes = lanes?;
            Ok(lanes != 0 && (0..list.len()).filter(|&p| live(p)).skip(h as usize).any(|p| granule_source::ranks_pending(&list, r, lanes, p)))
        };
        if let Some(h) = tail_from_part(live_parts, width).filter(|_| whole && !code_groups && !answerable && !sparse && !entry_runs) {
            if ranks(h)? {
                fires::fire("pgrc.tail_whole_ranks"); // shapecheck fire signal (D4732): the tail rule kept whole parts that rank a verbatim column
            } else if let Some(p) = (0..list.len()).filter(|&p| live(p)).nth(h as usize) {
                cursor.set_tail_from(list.starts()[p]);
                cursor.set_endgame_split(false);
                fires::fire("pgrc.claims_tail");
            }
        }
        // skewsplit (D3320): a running claim gives its back half to a worker that found the cursor exhausted, where no claim
        // is part-grain work (per-part code groups, an answerable site's part answers, a part's entry runs: whole, as the
        // tail rule keeps them); a walk's claims returned above, and a led section's never split (the pool)
        if crate::inputs::knobs::width::claim_split_on() && !code_groups && !answerable && !entry_runs && !sparse {
            cursor.set_split_running(true);
        }
        // D1536: an EXPLAIN ANALYZE execution's claims cover the dead parts too (`GranuleRecipe::skipped`): its source
        // skips each when its walk reaches it and counts the part's rows removed, so a LIMIT's early stop counts none past it
        if r.skipped.is_none() {
            cursor.set_dead(&dead);
        }
        resolved.install(r.rel, list);
        return Ok((Claims::Granules(cursor), dense, width, rows));
    }
    Err(glue::internal("gravity: parallel pipeline without a range recipe"))
}

impl Plan for RunnerPlan {
    fn rerun_sections(&self) -> bool {
        self.rerun_sections
    }

    fn width_of(&self, pipeline: usize) -> u16 {
        // M7-SPILL: a partition phase whose producer wrote no partition runs its width-1 arm, never a section with no claim
        if no_partition(&self.claims[pipeline]) && self.v4.claims_for(pipeline).is_none() { return 0; }
        // D3024 (b), sorted: an emit the owner rule keeps on the leader (`emit_on_leader`) runs its width-1 arm over its slices
        if self.sorted_emits && pipeline < 256 && self.emit_leader[pipeline >> 6].get() & (1u64 << (pipeline & 63)) != 0 {
            if self.widths[pipeline] >= 2 {
                fires::fire("exec.emit.leader"); // shapecheck fire signal (D3024 (b)): the owner rule kept a sorted wide emit on the leader
            }
            return 0;
        }
        // D3024 (b): a drain the owner rule keeps on the leader (`drain_on_leader`) runs its width-1 arm over its slices
        if let gravity_program::ClaimKind::BuildRecords { slot } = self.program.pipelines[pipeline].claims {
            if self.drain_leader[(slot >> 6) as usize].get() & (1u64 << (slot & 63)) != 0 {
                return 0;
            }
        }
        if let Some(w) = self.bitmaps.iter().find(|b| b.pipeline == pipeline).and_then(|b| b.width.get()) {
            return w.min(self.widths[pipeline]);
        }
        if let Some(w) = self.leaves.iter().find(|l| l.pipeline == pipeline).and_then(|l| l.width.get()) {
            return w.min(self.widths[pipeline]);
        }
        if let Some(w) = self.ranges.iter().find(|g| g.pipeline == pipeline).and_then(|g| g.width.get()) {
            return w.min(self.widths[pipeline]);
        }
        self.widths[pipeline]
    }

    fn claims_for(&self, pipeline: usize) -> &dyn MorselSource {
        self.sections.set(self.sections.get() + 1);
        if let gravity_program::ClaimKind::BuildRecords { .. } = self.program.pipelines[pipeline].claims {
            fires::fire("exec.drain.wide"); // shapecheck fire signal (D3024 (b)): a hash join build drain runs as a section over claims of its build's records
        }
        if let Some(c) = self.v4.claims_for(pipeline) { return c; }
        if pipeline == self.pass2_idx {
            if let Some(c) = self.pass2.get() {
                return c;
            }
        }
        match &self.claims[pipeline] {
            Claims::Blocks(b) => b,
            Claims::Granules(g) => g,
            Claims::Partitions(p, _) => p,
            Claims::Slices(s) => s,
            Claims::None => unreachable!("gravity: section without claims"),
        }
    }

    fn merge_for(&self, pipeline: usize) -> &dyn Merge {
        if let Some(m) = self.v4.merge_for(pipeline) { return m; }
        // D1236's held inner (op 14 for runs.sort, ops/sort/held.rs): a merge join's Sort-ended inner whose error waits for
        // the merge's first inner call; no runs.sort protocol answers it here (width 1, or no resident merge)
        if self.program.pipelines[pipeline].held_sort {
            return match self.program.pipelines[pipeline].held_at {
                true => &crate::ops::sort::held::HELD_AT,
                false => &crate::ops::sort::held::HELD_ROWS,
            };
        }
        match self.program.pipelines[pipeline].ends_in {
            End::Breaker(Breaker { kind: BreakerKind::PlainAgg, .. }) => &PLAIN,
            End::Breaker(Breaker { kind: BreakerKind::HashAgg | BreakerKind::Distinct, .. }) => &self.hash_aggs[self.hash_aggs.binary_search_by_key(&pipeline, |h| h.0).expect("gravity: a HashAgg pipeline's merge")].1,
            End::Breaker(Breaker { kind: BreakerKind::HashBuild, .. }) => &self.hash_build,
            End::Breaker(Breaker { kind: BreakerKind::TopK, .. }) => &self.top_k,
            _ => &ROWS,
        }
    }

    /// D899: `pipeline`'s late key words, evaluated as it starts: PostgreSQL's runtime keys at their scan's first pull.
    /// memsig E-pub: the words go in one block on the execution's prepare owner (`State::charged_vec`), which every
    /// protocol appends to (`Live::late_words`) and the pipeline's run drops (`gravity_parallel::with_late`).
    fn late_words(&self, pipeline: usize) -> gravity_driver::Result<gravity_parallel::LateWords> {
        // a pipeline with no late word of its own and no live that appends any opens no charge
        let mut out = match self.late.iter().any(|l| l.0 == pipeline) || self.v4.answered(crate::ops::ask::LATE) {
            true => self.v4.charged_vec(),
            false => gravity_parallel::LateWords::none(),
        };
        for &(_, w, e) in self.late.iter().filter(|l| l.0 == pipeline) {
            let exprs = super::raw::late_exprs(self.late_exprs);
            match admit::eval_late(exprs, e, self.late_mcx) {
                Ok((v, n)) => out.with_vec(|o| o.push((w, v, n))), // alloc: Exec charged v4.owner
                Err(err) => return Err(gravity_hosts::stash(err)),
            }
        }
        // M8 (L0-k): the v4 protocols' own (src.prune's exec pruning over in-program InitPlans)
        self.v4.late_words(pipeline, &mut out).map_err(gravity_hosts::stash)?;
        self.build_bitmap(pipeline, &out).map_err(gravity_hosts::stash)?;
        self.build_leaves(pipeline, &out).map_err(gravity_hosts::stash)?;
        self.build_ranges(pipeline, &out).map_err(gravity_hosts::stash)?;
        Ok(out)
    }

    /// M8 (RT-1): an Init's member runs only on demand (the v4 protocols' answer).
    fn deferred(&self, pipeline: usize) -> bool {
        self.v4.deferred(pipeline)
    }

    /// M8 (RT-2e): a rerun set's member's owner (the v4 protocols' answer).
    fn rerun_owner(&self, pipeline: usize) -> Option<usize> {
        self.v4.rerun_owner(pipeline)
    }

    /// M8 (RT-2f): a pulled CTE plan's readers (the v4 protocols' answer).
    fn pull_readers(&self, pipeline: usize) -> &[usize] {
        self.v4.pull_readers(pipeline).unwrap_or_default()
    }

    /// M8 (RT-2f): the data-modifying pulled sets the statement's end drains (the v4 protocols' answer).
    fn drain_at_end(&self) -> gravity_parallel::DrainSets<'_> {
        self.v4.drain_at_end()
    }

    /// D2289: a rerun set's re-open to the v4 protocols (the hosted aggregate arena's reset).
    fn reopen(&self, members: &[u16]) -> gravity_driver::Result<()> {
        // D2736: a re-opened pulled pipeline evaluates its late key words again at its next pull (PG's rescan)
        self.pulled.borrow_mut().retain(|(p, _)| !members.iter().any(|&m| m as usize == *p));
        self.v4.reopen(members).map_err(gravity_hosts::stash)
    }

    /// D2736: the late key words of pulled pipeline `pipeline`, evaluated at its first pull and kept for the later ones.
    fn pulled_late(&self, pipeline: usize) -> gravity_driver::Result<gravity_parallel::LateWords> {
        // memsig E-pub: each copy on the block's own owner (the execution's prepare owner)
        let copy = |w: &gravity_parallel::LateWords| {
            let mut c = self.v4.charged_vec();
            c.with_vec(|v| v.extend_from_slice(w)); // alloc: Exec charged v4.owner
            c
        };
        if let Some((_, w)) = self.pulled.borrow().iter().find(|(p, _)| *p == pipeline) {
            return Ok(copy(w));
        }
        let w = self.late_words(pipeline)?;
        self.pulled.borrow_mut().push((pipeline, copy(&w))); // alloc: Exec
        Ok(w)
    }

    /// M8 (RT-1): the runner's frame to the v4 protocols.
    fn frame(&self, f: Option<gravity_driver::Frame>) -> Option<gravity_driver::Frame> {
        self.v4.set_frame(f)
    }

    /// D2105: an arm this run's exec pruning removed (`Live::pruned_whole`).
    fn skips_whole(&self, pipeline: usize) -> bool {
        self.v4.pruned_whole(pipeline)
    }

    /// EXPLAIN ANALYZE: the cells the v4 protocols fill (a Memoize's cache counters).
    fn fill_cells(&self, cells: &mut [u64]) {
        self.v4.fill_cells(&self.program.instr, cells)
    }

    /// After `pipeline` completed: the memo's count, the leader's code-key fold, a top-k run's render.
    fn after_pipeline(&self, pipeline: usize, exec: &mut Exec) -> gravity_driver::Result<()> {
        let mut set_total = |slot: u8, n: u64| {
            for &(p, _) in self.partitions.iter().filter(|&&(_, s)| s == slot) {
                if let Claims::Partitions(range, _) = &self.claims[p] {
                    range.set_total(n);
                }
            }
        };
        // D546: the live life's parts: a slot reused after a spilled breaker's life ended (a build over an aggregate's slot)
        // keeps that life's parts in the census, never in the wall or the partition totals of the life that reuses it
        let parts_of = |e: &Exec, slot: u8| e.node().spill_parts_live(slot);
        // D3024 (b), sorted: each sorted wide emit still to run reads its producer's groups as the pipelines before it end
        // (the last before it decides: the producer's slot then holds the owner runs the emit claims)
        for q in self.program.v4.as_deref().filter(|_| self.sorted_emits).map_or(&[][..], |d| &d.protos[..]) {
            let k = q.pipeline as usize;
            if q.kind == gravity_program::protos::ProtoKind::EmitRuns && k > pipeline && k < 256 {
                let (word, bit) = (&self.emit_leader[k >> 6], 1u64 << (k & 63));
                word.set(if emit_on_leader(exec, &self.program, q) { word.get() | bit } else { word.get() & !bit });
            }
        }
        // D3024 (b): each drain still to run reads its build's size as the producers before it end (the last before it
        // decides: its slot then holds its own build); a slot holding no laid-out build opens a section
        for pipe in self.program.pipelines.iter().skip(pipeline + 1).filter(|_| self.drains) {
            if let gravity_program::ClaimKind::BuildRecords { slot } = pipe.claims {
                let (word, bit) = (&self.drain_leader[(slot >> 6) as usize], 1u64 << (slot & 63));
                word.set(if drain_on_leader(exec, slot, pipe.produces()).unwrap_or(false) { word.get() | bit } else { word.get() & !bit });
            }
        }
        if let Some(r) = producer_ran(&self.program.pipelines, pipeline, exec, parts_of, &mut set_total, |e| self.v4.after_pipeline(pipeline, e)) {
            return r;
        }
        if let (0, Some(key), Some(slot)) = (pipeline, self.key, self.program.pipelines[0].produces()) {
            let region = exec.node().slot_region(slot);
            if let Some((records, groups)) = gravity_abi::breaker::Owned::counts(region) {
                if let (true, Some(groups)) = (records > 0, groups) {
                    crate::admit::memo::groups_remember(key, (records, groups));
                }
            }
        }
        if self.two_pass && pipeline == self.pass1 {
            return self.build_admit_set(exec);
        }
        if pipeline == 0 && self.text_groups {
            return fold_codes_by_value(exec, self.program.pipelines[0].driving_site as usize, &self.params[..self.nparams]);
        }
        // a top-k run over code columns: render the codes to text through the leader's source and
        // order the kept ties by the secondary key's bytes
        if let (0, Some(Walk { groups: false, codes, secondary, .. }), End::Breaker(Breaker { kind: BreakerKind::TopK, slot, .. })) = (pipeline, self.walk, self.program.pipelines[0].ends_in) {
            if codes != 0 {
                let site = self.program.pipelines[0].driving_site as usize;
                let stmt = exec.lease();
                let node = exec.node_mut();
                let region = node.slot_region(slot).as_mut_ptr();
                let run = super::raw::topk_run(region);
                let src = node.source_mut(site).ok_or_else(|| gravity_driver::Error::host("gravity: top-k render without a source"))?;
                // a declared run's column is not its lane: the k-th code column reads byte k of the declaration's lanes
                let lanes = self.topk_lanes;
                let lane_of = move |col: u16| match lanes {
                    Some(w) => (w >> (8 * (codes & ((1u32 << col) - 1)).count_ones()) & 0xff) as u16,
                    None => col,
                };
                return order_topk(&stmt, run, codes, secondary, |part, col, code| src.dict_value(part, lane_of(col), code));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod d394_tests {
    use super::*;

    /// A rung's own source (a TID / TABLESAMPLE recipe) on a serial pipeline walks itself: no claim is cut (D394).
    struct Rung;
    impl gravity_hosts::recipe::HostRecipe for Rung {
        fn rel(&self) -> types_core::Oid { 1 }
        fn size_hint(&self) -> usize { 0 }
        fn derive(&self, _desc: &gravity_program::SiteDesc, _a: gravity_driver::ArenaRef) -> gravity_driver::SourceBox { unreachable!("never derived here") }
        fn clone_box(&self) -> Box<dyn gravity_hosts::recipe::HostRecipe> { Box::new(Rung) } // alloc: Cold
    }

    #[test]
    fn a_serial_rung_site_cuts_no_claims() {
        let mut reg = crate::shapes::empty_registry();
        assert!(!serial_host_site(&reg, 0), "does not apply: a site with no recipe is not a host walk");
        reg.push_rung(Box::new(Rung)); // alloc: Cold
        assert!(serial_host_site(&reg, 0), "a serial TID / TABLESAMPLE site walks its source itself");
        assert!(!serial_host_site(&reg, 1));
    }
}

#[cfg(test)]
mod m7_spill_tests {
    use super::*;

    /// M7-SPILL: a partition phase is unclaimed until its producer wrote a partition; a section claims the ones it wrote.
    #[test]
    fn a_partition_phase_with_no_partition_written_is_unclaimed() {
        let c = Claims::Partitions(gravity_parallel::PartitionRange::deferred(), 3);
        assert!(no_partition(&c), "sized 0 until its producer ran and wrote none");
        if let Claims::Partitions(r, _) = &c {
            r.set_total(32);
        }
        assert!(!no_partition(&c), "32 partitions written: the section claims them");
        assert!(!no_partition(&Claims::None), "does not apply: no partition phase");
    }
}

#[cfg(test)]
mod dr6_bound_tests {
    use super::*;
    use gravity_abi::breaker::{Budget, KeyKind, RecordLayout, SlotKind, Spill, ORDER_NONE, PARTITION_SLICE};
    use gravity_driver::mem::grant::{share, share_with};
    use gravity_program::protos::{Proto, ProtoKind, V4Descs};
    use gravity_program::{Arena, Code, Layout, OwnerDesc, Pipeline, Reason, SiteDesc, WritePlan};

    /// The bind as DR-6 step 2 (8e6fae20cf1) landed it, `grant::share` per pipeline: the reference `Bound` must equal.
    fn today(program: &Program, grant: Grant, slice: u64, i: usize, w: u16, bits: u8, rows: Option<(u64, u32)>) -> u8 {
        use gravity_abi::breaker::{granted_bits, machine_bits, partition_bits, t1_default_bytes};
        let bits = match rows {
            Some((r, rec)) => partition_bits(r, rec as u64, slice),
            None => machine_bits(bits, slice),
        };
        if program.pipelines[i].breaker().is_some_and(|b| b.kind == BreakerKind::HashBuild) {
            return bits;
        }
        let s = share(program, i, grant, w);
        granted_bits(bits, s.worker.bytes as u64, t1_default_bytes() as u64)
    }

    /// xorshift64*, the test's own stream (no seed from the clock: a failure names its program).
    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
        }
    }

    fn grants() -> [Grant; 5] {
        [Grant::of(64 << 10), Grant::of(1 << 20), Grant::of(64 << 20), Grant::of(1 << 30), Grant::from_gucs(4096, 2.0).with_floors(4 << 20, 8 << 20)]
    }

    /// `n` producers (each kind, partitioned or not, share- or fixed-budgeted, over slots 0..4), half of them with a generic
    /// owner whose layout pins group images (a MinVar / MaxVar slot) when `images`, then the sink.
    fn random(rng: &mut Rng, n: usize, images: bool) -> Program {
        let st = Arena::new(1 << 14);
        let sites = [SiteDesc::range_source(st.handle(), 7, &[1])];
        let layout = Layout { nsites: 1, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
        let kinds = [BreakerKind::PlainAgg, BreakerKind::HashAgg, BreakerKind::HashBuild, BreakerKind::Sort, BreakerKind::Distinct, BreakerKind::TopK];
        let extra = [SlotKind::Count, SlotKind::SumI64, SlotKind::MinVar, SlotKind::MaxVar];
        let mut pipes: Vec<Pipeline> = Vec::with_capacity(n + 1); // alloc: Cold
        for _ in 0..n {
            let spill = if rng.below(3) == 0 { Spill::None } else { Spill::Partitioned { bits: rng.below(13) as u8 } };
            let budget = if rng.below(4) == 0 { Budget::Fixed(64u32 << rng.below(12)) } else { Budget::Share };
            let b = Breaker { kind: kinds[rng.below(kinds.len() as u64) as usize], slot: rng.below(4) as u8, bytes: 64, budget, spill };
            let mut p = Pipeline::eligible(0, End::Breaker(b), 1000);
            if rng.below(2) == 0 {
                let slot = extra[rng.below(if images { 4 } else { 2 }) as usize];
                p = p.owner(OwnerDesc::generic(RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, slot]), ORDER_NONE, 0));
            }
            pipes.push(p);
        }
        pipes.push(Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
        Program::with_pipelines(layout, &sites, &[], WritePlan::SELECT, pipes.into_iter().map(|p| (p, Code::hand(crate::ops::ctescan::raw::test_noop))))
    }

    /// crt-0384's form: `arms` partitioned hash aggregates into slot 0, each with its generic owner (the last one's layout
    /// pinning images when `images`), one Arms run over them, then the emit.
    fn arms(n: usize, images: bool) -> Program {
        let st = Arena::new(1 << 14);
        let sites = [SiteDesc::range_source(st.handle(), 3840, &[1, 2])];
        let layout = Layout { nsites: 1, out_natts: 0, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 };
        let b = End::Breaker(Breaker { kind: BreakerKind::HashAgg, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits: 3 } });
        let owner = |last: bool| {
            let slots: &[SlotKind] = if last && images { &[SlotKind::Count, SlotKind::MinVar] } else { &[SlotKind::Count] };
            OwnerDesc::generic(RecordLayout::new(&[KeyKind::I32], slots), ORDER_NONE, 0)
        };
        let mut pipes: Vec<Pipeline> = (0..n).map(|a| Pipeline::eligible(0, b, 1000).owner(owner(a + 1 == n))).collect(); // alloc: Cold
        pipes.push(Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(0));
        let p = Program::with_pipelines(layout, &sites, &[], WritePlan::SELECT, pipes.into_iter().map(|p| (p, Code::hand(crate::ops::ctescan::raw::test_noop))));
        p.with_v4(V4Descs::new(vec![Proto::new(ProtoKind::Arms, 0).slot(0).arg(n as u64)])) // alloc: Cold
    }

    /// The images flag read once gives every pipeline today's share and today's bits: random programs, each kind,
    /// with and without image slots, at every width, grant, slice and memo form.
    #[test]
    fn the_once_read_bind_equals_todays_over_random_programs() {
        let slices = [PARTITION_SLICE, 64 << 10, 256 << 10, 1 << 20, 3 << 20];
        let (mut checked, mut with_images) = (0u64, 0u32);
        for seed in 1..=96u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let n = 1 + rng.below(24) as usize;
            let p = random(&mut rng, n, seed % 2 == 0);
            let images = gravity_driver::program_images(&p);
            with_images += images as u32;
            for grant in grants() {
                for w in [0u16, 1, 2, 3, 4, 8, 16, 64] {
                    for i in 0..p.pipelines.len() {
                        assert_eq!(share_with(&p, i, grant, w, images), share(&p, i, grant, w), "seed {seed} pipeline {i} W{w} {grant:?}");
                    }
                    for slice in slices {
                        let bound = Bound::new(&p, grant, slice);
                        for i in 0..p.pipelines.len() {
                            let bits = match p.pipelines[i].breaker().map(|b| b.spill) {
                                Some(Spill::Partitioned { bits }) => bits,
                                _ => rng.below(13) as u8,
                            };
                            let rows = (rng.below(2) == 0).then(|| (rng.below(1 << 30), 8 + rng.below(504) as u32));
                            assert_eq!(bound.bits(i, w, bits, rows), today(&p, grant, slice, i, w, bits, rows), "seed {seed} pipeline {i} W{w} {grant:?} slice {slice} bits {bits} rows {rows:?}");
                            checked += 1;
                        }
                    }
                }
            }
        }
        assert!(with_images > 8 && with_images < 88, "both forms drawn: {with_images} of 96 programs pin images");
        assert!(checked > 100_000, "{checked} binds checked");
    }

    /// crt-0384 / mpart-0002's shape, 4150 partition arms (the O(pipelines^2) bind): every arm's share and bits equal
    /// today's, with and without an image slot in the last arm's owner.
    #[test]
    fn the_4150_arm_bind_equals_todays() {
        for images in [false, true] {
            let p = arms(4150, images);
            assert_eq!(gravity_driver::program_images(&p), images);
            for (grant, w) in [(Grant::of(64 << 20), 1u16), (Grant::from_gucs(4096, 2.0).with_floors(4 << 20, 8 << 20), 8)] {
                let bound = Bound::new(&p, grant, PARTITION_SLICE);
                for i in 0..p.pipelines.len() {
                    assert_eq!(share_with(&p, i, grant, w, images), share(&p, i, grant, w), "images {images} arm {i} W{w}");
                    assert_eq!(bound.bits(i, w, 3, None), today(&p, grant, PARTITION_SLICE, i, w, 3, None), "images {images} arm {i} W{w}");
                }
                for i in [0, 1, 2074, 4149, 4150] {
                    assert_eq!(bound.bits(i, w, 0, Some((4, 16))), today(&p, grant, PARTITION_SLICE, i, w, 0, Some((4, 16))), "images {images} memo arm {i} W{w}");
                }
            }
        }
    }
}

#[cfg(test)]
mod runs02_tests {
    use super::*;

    /// RUNS-02 / PL-05: the run-time census re-decides as the planner's price does: a group walk over 1,000 granules
    /// (one key each: spread 0) for n keys reaches n of them; it stays a walk while `Walk::group_share` is under 1 and
    /// takes the plain claims from there, at W0, W2 and W8; a spread key (every granule spanning it) is cheaper.
    #[test]
    fn a_group_walk_past_its_price_takes_the_plain_claims() {
        let mins: Vec<i64> = (0..1_000).collect(); // alloc: Cold
        let walk = gravity_program::cost::walk::Walk::of(gravity_program::cost::walk::Profile::parse(None));
        let last = |g: u32, w: u32, spread: f64| {
            let price = |n: u32| walk.group_share(u64::from(n), u64::from(g), u64::from(n), u64::from(n), spread, 0.0, w);
            (1..g).take_while(|&n| price(n) < 1.0).last().expect("a walk under its price")
        };
        for w in [0, 2, 8] {
            let e = last(1_000, w, 0.0);
            assert!(!group_walk_dearer(&mins, e, 1_000, w as u16, 0.0) && group_walk_dearer(&mins, e + 1, 1_000, w as u16, 0.0), "W{w}: {e}");
            assert!(!group_walk_dearer(&mins, e + 1, 1_000, w as u16, 1.0), "W{w}: a spread key walks further");
        }
        assert!(last(1_000, 8, 0.0) < last(1_000, 0, 0.0), "the W8 term decides first");
        let mut dup = mins.clone(); // alloc: Cold
        dup.splice(0..0, core::iter::repeat_n(0, 1_000)); // a second part whose every granule holds key 0
        assert!(group_walk_dearer(&dup, 300, 2_000, 0, 0.0), "1,300 of 2,000 granules reach the 300th distinct minimum");
        assert!(!group_walk_dearer(&mins, 300, 2_000, 0, 0.0), "300 of them do not");
    }

    /// PL-05: the spread from the two zone orders: one key a granule reads 0, granules spanning the key read near 1, a
    /// keyless granule is left out.
    #[test]
    fn the_spread_reads_the_zone_orders() {
        let one: Vec<i64> = (0..100).collect(); // alloc: Cold
        let desc_one: Vec<i64> = (0..100).rev().map(|k: i64| !k).collect(); // alloc: Cold
        assert_eq!(zone_spread(&one, &desc_one), 0.0);
        let wide_desc: Vec<i64> = vec![!1_000_000i64; 100]; // alloc: Cold
        assert!(zone_spread(&one, &wide_desc) > 0.99);
        let mut keyless = one.clone(); // alloc: Cold
        keyless.insert(0, i64::MIN);
        let mut keyless_desc = desc_one.clone(); // alloc: Cold
        keyless_desc.push(i64::MIN);
        assert_eq!(zone_spread(&keyless, &keyless_desc), 0.0);
    }
}
