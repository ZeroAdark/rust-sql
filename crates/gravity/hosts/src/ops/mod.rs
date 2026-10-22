use core::mem::size_of;
use std::rc::Rc;

use gravity_driver::{box_locker, box_writer, ArenaBox, ArenaRef, Derive, Locker, SourceBox, Writer};
use gravity_program::{SiteDesc, WritePlan};

use crate::ops::scan::pgrc::fetch_site::FetchRecipe;
use crate::ops::scan::pgrc::granule_source::GranuleRecipe;
use crate::ops::scan::heap_source::RangeRecipe;
use crate::ops::scan::index_source::SiteRecipe;
use crate::recipe::{DeriveFn, HostRecipe};
use crate::ops::lockrows::lock::LockHost;
use crate::ops::modifytable::write::WriteHost;
use crate::ops::modifytable::trigger_stmt::MergeOps;
use crate::ops::modifytable::write_check::CheckSpec;
use crate::ops::modifytable::write_route::{MultiTargetHost, RouteHost};
use crate::ops::modifytable::write_constraints::ModifiedCols;
pub mod agg;
pub mod group;
pub mod lockrows;
pub mod memoize;
pub mod modifytable;
pub mod scan;
pub mod shared;
pub mod sort;
pub mod srf;
pub mod subplan;
pub mod window;

#[derive(Clone)]
pub enum Recipe {
    Index(SiteRecipe),
    /// The bitmap walk over an index recipe (`bitmap_source.rs`), its recheck qual compiled per execution.
    Bitmap(SiteRecipe, Option<crate::FilterFn>),
    Range(RangeRecipe),
    Granule(GranuleRecipe),
    Fetch(FetchRecipe),
    /// A later rung's recipe kind (`recipe.rs`), derived by its own table.
    Rung(Box<dyn HostRecipe>),
}

/// Each rung's derive and its sources' largest size (lowerv3 design §7.4): one line per rung, written once; a rung that
/// lands replaces its empty entry.
const DERIVES: &[(DeriveFn, usize)] = &[(crate::ops::scan::pgrc::derive::derive, crate::ops::scan::pgrc::derive::SIZE), (crate::ops::scan::derive::derive, crate::ops::scan::derive::SIZE), (crate::ops::scan::pgrc::derive_fetch::derive, crate::ops::scan::pgrc::derive_fetch::SIZE), (crate::v4::derive, crate::v4::SIZE)];

pub struct HostRegistry {
    /// D3253: this registry's identity (`Derive::identity`), unique in the process.
    id: u64,
    recipes: Vec<Recipe>,
    next: usize,
    modified: Option<ModifiedCols>,
    checks: Option<Rc<[CheckSpec]>>,
    merge: MergeOps,
    /// D1687: each writing CTE's modified columns by its relation and command, for a writer beside the program's `write`;
    /// D3623: and its ModifyTable's plan node.
    modified_by_rel: Vec<(u32, gravity_program::Cmd, i32, ModifiedCols)>,
    /// D1687: each writer beside the program's `write`, by its relation: its own check options and MERGE subcommands.
    beside: Vec<Beside>,
    /// D1920 / D1644: each sample plan node's seed in this execution (`tid_sample::SampleRecipe::node`), shared by its sites.
    /// memsig E-pub t4: the list and each seed's block charged to `charge_owner` (`charge_to`: the binding backend's session).
    sample_seeds: memledger::ChargedVec<(usize, crate::ops::scan::tid_sample::SampleSeed)>,
    charge_owner: crate::ops::scan::heap_source::HeldOwner,
    /// D3438: this backend's host state for a program filed without one (`Derive::owner`).
    owner: Option<Rc<dyn core::any::Any>>,
    /// D3615 (a): the largest rung recipe's source size, kept as recipes are pushed (`size_hint`, asked once per site, walked
    /// every recipe: 16 M steps per Node at 4000 partitions)
    rung_hint: usize,
}

/// D1687 (exec ops/modifytable/beside.rs): a writer beside the program's `write`: its relation (a route's root, else its
/// target), its check options, its MERGE subcommands.
pub type Beside = (u32, Option<Rc<[CheckSpec]>>, MergeOps);

impl HostRegistry {
    pub fn new() -> HostRegistry {
        static NEXT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
        let id = NEXT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        HostRegistry { id, recipes: Vec::new(), next: 0, modified: None, checks: None, merge: MergeOps::NONE, modified_by_rel: Vec::new(), beside: Vec::new(), sample_seeds: memledger::ChargedVec::none(), charge_owner: None, owner: None, rung_hint: 0 } // alloc: Program
    }

    /// D3438: the host state a Node built from this registry runs its program over (the program's own when unset).
    pub fn set_owner(&mut self, owner: Option<Rc<dyn core::any::Any>>) {
        self.owner = owner;
    }

    pub fn owner_of(&self) -> Option<&dyn core::any::Any> {
        self.owner.as_deref()
    }

    /// The result relation's `insertedCols` / `updatedCols` of THIS statement (a cached program's next statement has its own).
    pub fn set_modified_cols(&mut self, cols: Option<ModifiedCols>) {
        self.modified = cols;
    }

    /// D1687: THIS statement's writing CTEs' modified columns, each by its relation (a writing CTE's own target) and its
    /// command (D3348: an INSERT and an UPDATE of one relation each find their own); D3623: and by its ModifyTable's plan
    /// node, which a writer naming its node finds first (two UPDATEs of one relation each find their own).
    pub fn set_modified_by_rel(&mut self, cols: Vec<(u32, gravity_program::Cmd, i32, ModifiedCols)>) {
        self.modified_by_rel = cols;
    }

    /// D1687: THIS statement's writers beside the program's `write`, each with its own check options and MERGE subcommands.
    pub fn set_beside(&mut self, facts: Vec<Beside>) {
        self.beside = facts;
    }

    /// H-f (V4R-239): THIS statement's MERGE subcommands (`mt_merge_subcommands`, from its `mergeActionLists`).
    pub fn set_merge_ops(&mut self, ops: MergeOps) {
        self.merge = ops;
    }

    /// THIS statement's WITH CHECK OPTION / RLS check quals (`write_check.rs`), relocated per admission.
    pub fn set_checks(&mut self, checks: Vec<CheckSpec>) {
        self.checks = Some(Rc::from(checks)); // alloc: Program
    }

    pub fn push(&mut self, r: SiteRecipe) {
        self.push_recipe(Recipe::Index(r));
    }

    pub fn push_bitmap(&mut self, r: SiteRecipe, recheck: Option<crate::FilterFn>) {
        self.push_recipe(Recipe::Bitmap(r, recheck));
    }

    pub fn push_range(&mut self, r: RangeRecipe) {
        self.push_recipe(Recipe::Range(r));
    }

    pub fn push_granule(&mut self, r: GranuleRecipe) {
        self.push_recipe(Recipe::Granule(r));
    }

    /// A by-rowid fetch site (fetch.md §1.2): a `RangeSource` descriptor whose lanes are filled by `Site.fetch`.
    pub fn push_fetch(&mut self, r: FetchRecipe) {
        self.push_recipe(Recipe::Fetch(r));
    }

    /// D1920 / D1644: sample plan node `node`'s seed in this execution (`tid_sample::SampleRecipe::node`), one per node.
    pub fn sample_seed(&mut self, node: usize) -> crate::ops::scan::tid_sample::SampleSeed {
        if let Some((_, s)) = self.sample_seeds.iter().find(|(n, _)| *n == node) {
            return s.clone();
        }
        // the list opens its charge at its first seed (a registry with no sample opens none)
        if self.sample_seeds.is_empty() && self.sample_seeds.owner() != self.charge_owner.map(|o| o.0) {
            self.sample_seeds = crate::ops::scan::heap_source::charged_scratch(self.charge_owner);
        }
        let s = crate::ops::scan::tid_sample::new_seed(self.charge_owner);
        let kept = s.clone();
        self.sample_seeds.with_vec(|v| v.push((node, kept))); // alloc: Exec charged owner
        s
    }

    /// memsig E-pub t4 (a3's condition via memacct 10-16): the owner this registry charges what it grows at bind (the sample
    /// seeds list and each seed's block): the SESSION of the binding statement's backend (`BindCx::owner`'s account), kind
    /// as given. The registry lives as long as its admission, and a served or kept admission (the memo's plan slots, admit
    /// `kept`) carries it, its seeds and the parked sources sharing them past the statement's end; the session outlives
    /// every admission of its backend, so no statement's end finds these bytes on it (no owner-end residue) and none go
    /// uncharged. Each seed's bytes leave with its last `Rc` (the registry's or a parked source's), the list's with the
    /// registry, on the dropping thread. Set before the first recipe binds.
    pub fn charge_to(&mut self, owner: Option<(memledger::Owner, memledger::Kind)>) {
        self.charge_owner = owner.map(|(o, k)| (memledger::Owner::Session(o.acct()), k));
    }

    /// A later rung's recipe (`recipe.rs`), derived by itself.
    pub fn push_rung(&mut self, r: Box<dyn HostRecipe>) {
        self.push_recipe(Recipe::Rung(r));
    }

    fn push_recipe(&mut self, r: Recipe) {
        if let Recipe::Rung(h) = &r {
            self.rung_hint = self.rung_hint.max(h.size_hint());
        }
        self.recipes.push(r); // alloc: Program
    }

    pub fn len(&self) -> usize {
        self.recipes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.recipes.is_empty()
    }

    /// Entry `i`'s index recipe, the park salt's (exec `park_salt`). A bitmap entry answers `None`: its recheck qual is
    /// compiled in THIS execution's context (its params are the statement's), so a Node holding it must never park.
    pub fn recipe(&self, i: usize) -> Option<&SiteRecipe> {
        match self.recipes.get(i) {
            Some(Recipe::Index(r)) => Some(r),
            _ => None,
        }
    }

    /// Entry `i`'s one-index bitmap recipe (`Recipe::Bitmap`), for its park salt (exec runner/drive.rs `park_salt`): its
    /// source takes each execution's recheck qual at its bind (`recheck`), so a parked Node serves it. A tree's or a
    /// rung's bitmap is not one.
    pub fn bitmap_site(&self, i: usize) -> Option<&SiteRecipe> {
        match self.recipes.get(i) {
            Some(Recipe::Bitmap(r, _)) => Some(r),
            _ => None,
        }
    }

    /// Entry `i`'s recheck qual (`bitmapqualorig`, this execution's), for the statement's `Resolved` (`cells_from`).
    pub fn bitmap_recheck(&self, i: usize) -> Option<&crate::FilterFn> {
        match self.recipes.get(i) {
            Some(Recipe::Bitmap(_, f)) => f.as_ref(),
            _ => None,
        }
    }

    /// M12 Row B: entry `i`'s bitmap recipe, read for its counters only (hosts v4/m12.rs; never a park salt).
    pub fn bitmap_recipe(&self, i: usize) -> Option<&SiteRecipe> {
        match self.recipes.get(i) {
            Some(Recipe::Bitmap(r, _)) => Some(r),
            Some(Recipe::Rung(r)) => match r.bitmap_morsel().map(|m| &m.bitmap) {
                Some(crate::ops::scan::bitmap_morsel::MorselBitmap::Leaf(l)) => Some(l),
                _ => None,
            },
            _ => None,
        }
    }

    /// D3014O: entry `i`'s worker probe (an index recipe's worker form), whose image the runner captures per execution.
    pub fn worker_probe(&self, i: usize) -> Option<&crate::ops::scan::index_probe::WorkerProbe> {
        match self.recipes.get(i) {
            Some(Recipe::Index(r)) => r.worker(),
            _ => None,
        }
    }

    /// Entry `i`'s index recipe when it has a leaf form (an index-only site the pool reads, `SiteRecipe::with_leaves`).
    pub fn leaf_site(&self, i: usize) -> Option<&crate::ops::scan::index_source::SiteRecipe> {
        match self.recipes.get(i) {
            Some(Recipe::Index(r)) if r.leaves().is_some() => Some(r),
            _ => None,
        }
    }

    /// Entry `i`'s index recipe when it has a ranges form (an ordered site the pool reads in key ranges,
    /// `SiteRecipe::with_ranges`).
    pub fn range_site(&self, i: usize) -> Option<&crate::ops::scan::index_source::SiteRecipe> {
        match self.recipes.get(i) {
            Some(Recipe::Index(r)) if r.ranges().is_some() => Some(r),
            _ => None,
        }
    }

    pub fn range_recipe(&self, i: usize) -> Option<&RangeRecipe> {
        match self.recipes.get(i) {
            Some(Recipe::Range(r)) => Some(r),
            _ => None,
        }
    }

    /// Entry `i`'s bitmap morsel recipe (a rung recipe), whose claims the runner builds.
    pub fn bitmap_morsel(&self, i: usize) -> Option<&crate::ops::scan::bitmap_morsel::BitmapMorselRecipe> {
        match self.recipes.get(i) {
            Some(Recipe::Rung(r)) => r.bitmap_morsel(),
            _ => None,
        }
    }

    pub fn granule_recipe(&self, i: usize) -> Option<&GranuleRecipe> {
        match self.recipes.get(i) {
            Some(Recipe::Granule(r)) => Some(r),
            _ => None,
        }
    }

    /// Entry `i`'s by-rowid fetch recipe (a late-materialization site, `fetch_site.rs`).
    pub fn fetch_recipe(&self, i: usize) -> Option<&FetchRecipe> {
        match self.recipes.get(i) {
            Some(Recipe::Fetch(r)) => Some(r),
            _ => None,
        }
    }

    /// Every relation a recipe reads (a relation may repeat).
    pub fn relids(&self) -> impl Iterator<Item = types_core::Oid> + '_ {
        self.recipes.iter().map(|r| match r {
            Recipe::Index(r) | Recipe::Bitmap(r, _) => r.heap,
            Recipe::Range(r) => r.heap,
            Recipe::Granule(r) => r.rel,
            Recipe::Fetch(r) => r.rel,
            Recipe::Rung(r) => r.rel(),
        })
    }

    pub fn rewind(&mut self) {
        self.next = 0;
    }
}

impl Derive for HostRegistry {
    fn identity(&self) -> u64 {
        self.id
    }

    fn owner(&self) -> Option<Rc<dyn core::any::Any>> {
        self.owner.clone()
    }

    /// D3128: the index recipes' counter cells (`SiteRecipe::cells`), folded in registry order.
    fn cells(&self) -> u64 {
        self.recipes.iter().fold(0, |h, r| match r {
            Recipe::Index(s) | Recipe::Bitmap(s, _) => (h ^ s.cells()).wrapping_mul(0x9e37_79b9_7f4a_7c15),
            _ => h,
        })
    }

    fn size_hint(&self, desc: &SiteDesc) -> usize {
        // sortmerge T2: a merged site's source reserves its live windows' room at its build (merged.rs `footprint`)
        let merged = if desc.is_merged() { crate::ops::scan::pgrc::merged::footprint(desc) } else { 0 };
        DERIVES.iter().map(|d| d.1).max().unwrap_or(0).max(self.rung_hint).max(merged)
    }

    fn derive_source(&mut self, desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        assert!(!self.recipes.is_empty(), "gravity: derive without recipes");
        let i = self.next % self.recipes.len();
        self.next += 1;
        let r = &self.recipes[i];
        if let Recipe::Rung(h) = r {
            return h.derive(desc, a);
        }
        match DERIVES.iter().find_map(|d| (d.0)(r, i, desc, a)) {
            Some(s) => s,
            None => panic!("gravity: recipe {i} has no rung that derives it"),
        }
    }

    /// D3014O: a worker's copy of a site a worker probes: its recipe's worker form (`index_probe.rs`), else the leader's
    /// form (which a worker's bind traps; exec's run keeps such an execution on the leader).
    fn derive_worker_source(&mut self, desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        assert!(!self.recipes.is_empty(), "gravity: derive without recipes");
        let i = self.next % self.recipes.len();
        let attrs = crate::ops::scan::index_probe::lanes_served(desc);
        if let Some(src) = self.worker_probe(i).filter(|_| desc.worker_probe() && attrs).map(|p| crate::ops::scan::index_probe::derive(p, i, a)) {
            self.next += 1;
            return src;
        }
        // a leaves site's worker: its share of the run's leaf walk (index_leaves.rs)
        if let Some(src) = self.leaf_site(i).and_then(|r| r.leaves()).filter(|_| desc.leaves() && attrs).map(|l| crate::ops::scan::index_leaves::derive(l, i, a)) {
            self.next += 1;
            return src;
        }
        // a ranges site's worker: its claims' ranges (index_ranges.rs)
        if let Some(src) = self.range_site(i).and_then(|r| r.ranges()).filter(|_| desc.ranges() && attrs).map(|g| crate::ops::scan::index_ranges::derive(g, i, a)) {
            self.next += 1;
            return src;
        }
        self.derive_source(desc, a)
    }

    fn writer_size_hint(&self) -> usize {
        size_of::<WriteHost>().max(size_of::<RouteHost>()).max(size_of::<MultiTargetHost>())
    }

    fn derive_writer(&mut self, plan: &WritePlan, a: ArenaRef) -> ArenaBox<dyn Writer> {
        self.next = 0;
        // V4R-498: a MERGE whose only writes are NOT MATCHED INSERTs routes them as an INSERT (its statement triggers MERGE's)
        let merge_ins = plan.cmd == gravity_program::Cmd::Merge && !self.merge.update && !self.merge.delete;
        if plan.route.is_some() && (plan.cmd == gravity_program::Cmd::Insert || merge_ins) {
            return box_writer(RouteHost::new(plan).with_modified(self.modified).with_checks(self.checks.clone()).with_merge(self.merge), a);
        }
        // H-e (b): several result relations (the route's leaves; a pruned single-leaf write has none: WriteHost's under_root);
        // W6-MERGE-ROUTE-MATCHED: a MERGE that updates or deletes (its INSERT-only form took RouteHost above)
        if plan.route.as_ref().is_some_and(|r| !r.leaves.is_empty()) && matches!(plan.cmd, gravity_program::Cmd::Update | gravity_program::Cmd::Delete | gravity_program::Cmd::Merge) {
            return box_writer(MultiTargetHost::new(plan).with_modified(self.modified).with_checks(self.checks.clone()).with_merge(self.merge), a);
        }
        box_writer(WriteHost::new(plan).with_modified(self.modified).with_checks(self.checks.clone()).with_merge(self.merge), a)
    }

    /// D1687: a writing CTE's writer reads its own result relation's modified columns (ExecConstraints' DETAIL).
    /// D3623: its own ModifyTable's when it names its plan node, else the one of its relation and command.
    fn derive_writer_beside(&mut self, plan: &WritePlan, node: i32, a: ArenaRef) -> ArenaBox<dyn Writer> {
        let rel = plan.route.as_ref().map_or(plan.target_rel, |r| r.root);
        let mine = self.modified_by_rel.iter().find(|e| node != 0 && e.2 == node);
        let own = mine.or_else(|| self.modified_by_rel.iter().find(|(r, c, _, _)| *r == rel && *c == plan.cmd)).map(|e| e.3);
        let main = core::mem::replace(&mut self.modified, own);
        // its own check options and, a MERGE, its own subcommands, when the statement gave them (`set_beside`)
        let facts = self.beside.iter().find(|f| f.0 == rel).map(|f| (f.1.clone(), f.2));
        let checks = facts.as_ref().map(|f| core::mem::replace(&mut self.checks, f.0.clone()));
        let merge = facts.filter(|_| plan.cmd == gravity_program::Cmd::Merge).map(|f| core::mem::replace(&mut self.merge, f.1));
        let w = self.derive_writer(plan, a);
        self.modified = main;
        if let Some(c) = checks {
            self.checks = c;
        }
        if let Some(m) = merge {
            self.merge = m;
        }
        w
    }

    fn locker_size_hint(&self) -> usize {
        size_of::<LockHost>()
    }

    fn derive_locker(&mut self, plan: &WritePlan, a: ArenaRef) -> ArenaBox<dyn Locker> {
        self.next = 0;
        box_locker(LockHost::new(plan), a)
    }
}

#[cfg(test)]
mod memsig_t4_tests {
    /// memsig E-pub t4 witness (a3's condition via memacct 10-16): a registry bound in a statement holds its sample seeds on
    /// that backend's session, never on the statement, so the statement's end (a served or kept admission carries the
    /// registry past it) moves nothing; the bytes leave with the registry and the seed's last `Rc`. A registry with no sample
    /// opens no charge.
    #[test]
    fn a_registrys_sample_seeds_sit_on_the_session_past_the_statement() {
        use memledger::{charge, Kind, Owner};
        std::thread::spawn(|| {
            memledger::bind_backend(3961, 0);
            memledger::statement_begin();
            let (stmt, _) = charge::context();
            let session = Owner::Session(stmt.acct());
            let c = |o| charge::owner_row(o).committed;
            let s0 = c(session);
            let mut idle = super::HostRegistry::new();
            idle.charge_to(Some((stmt, Kind::Region)));
            assert_eq!((c(stmt), c(session) - s0, idle.sample_seeds.owner()), (0, 0, None), "no sample: no charge opened");
            let mut reg = super::HostRegistry::new();
            reg.charge_to(Some((stmt, Kind::Region)));
            let seed = reg.sample_seed(7);
            assert!(std::rc::Rc::ptr_eq(&seed, &reg.sample_seed(7)), "one seed per plan node");
            let held = c(session) - s0;
            assert!(held > 0, "the seed's block and the list are the session's");
            assert_eq!(c(stmt), 0, "none on the statement");
            memledger::statement_end();
            assert_eq!(c(session) - s0, held, "the statement's end moves nothing");
            drop(reg);
            assert!(c(session) - s0 > 0 && c(session) - s0 < held, "the list left with the registry, the seed stays with its Rc");
            drop(seed);
            assert_eq!(c(session), s0, "the seed's bytes leave with its last Rc");
            drop(idle);
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }
}
