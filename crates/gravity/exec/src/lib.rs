//! Gravity exec: the statement runner tcop calls (docs/gravity/exec.md).
#![deny(unsafe_code)]

pub mod admit;
/// The effects home's lower layer in exec: PostgreSQL's own volatility reading of a plan expression.
pub(crate) mod effects;
pub mod glue;
pub mod invalidate;
pub mod legs;
pub mod output_cid;
pub mod receivers;
pub mod phases;
pub mod portal_snapshot;
pub mod runner;
pub mod shapes;

use std::collections::HashMap;

use pgsync::OnceLock;
use std::sync::Arc;

use gravity_driver::Entry;
use gravity_program::{Arena, ContentKey, Handle, PlanKey, Program, ProgramCache};

pub use crate::admit::memo::{PlanMemo, Verdict};
pub use runner::ExecSlot;
pub use shapes::{Admitted, KeySrc};
/// mem's root context (rule C14: only mem makes a root), for the crates above exec that cannot name the driver (prewarm).
pub use gravity_driver::mem::root::Root;
/// Memory step 6a: the broker's view functions (seams_init installs them on the reserved oids 9070 / 9071).
pub use gravity_hosts::membroker::view::BROKER_BUILTINS as MEMBROKER_BUILTINS;
pub use gravity_program::cost::runtime::{MAX_EXECS, MAX_PLANS, PREWARM_IMAGE_CHUNK};
pub mod inputs;
pub mod ops;
pub mod pulled;
mod raw;
pub mod rerun;
pub mod statement;
pub mod table;

pub const PARK_NODES: usize = 64;

/// stmtoh F6 (`PGRUST_GRAVITY_PARK_PLANS`, default on): the Nodes a static statement level parks, as many as the plans it
/// keeps (its plan slots: `MAX_PLANS`, fewer under the e2e memo-slots knob, never fewer than `PARK_NODES`); off,
/// `PARK_NODES`. The park's byte cap (1/11 of the pool, driver `park_bytes`) is unchanged and still bounds what it holds.
/// At 64 a backend whose statements have more shapes than that rebuilt and dropped a Node at every count eviction: sysbench
/// TPC-C over 20 tables evicted at 27 % of its puts (18 KiB Nodes on average, 1 MiB parked under a 496 MiB cap).
pub fn park_nodes(nplans: usize, on: bool) -> usize {
    if on { nplans.max(PARK_NODES) } else { PARK_NODES }
}
/// D3465: a level past the static ones (`admit::reentry::MAX_DEPTH`) runs one statement of a deep nest (a SQL function
/// recursing as deep as max_stack_depth lets it), so its tables are sized for the few plans it files: a level sized as the
/// static ones (park 64 Nodes, memo 4,096 keys, 256 plan slots) holds ~150 KB before it runs anything, and 47,000 levels
/// held 11 GB. A smaller table evicts sooner: one admission or build again, never a wrong answer.
pub const DEEP_PARK_NODES: usize = 4;
pub const DEEP_PLANS: usize = 8;
pub const DEEP_MEMO_SETS: usize = 1;

pub struct Served {
    pub key: PlanKey,
    pub served: Box<Admitted>,
}

/// The program installer (milestone-2.md §2.6 step 2): another program of the same `Program::front_key` to run instead.
pub type Installer = fn(&Admitted) -> Option<Arc<Program>>;

static INSTALLER: OnceLock<Installer> = OnceLock::new();

pub fn set_installer(f: Installer) {
    let _ = INSTALLER.set(f);
}

pub fn installer() -> Option<Installer> {
    INSTALLER.get().copied()
}

/// D2845: the admission's refusal when the address space cannot hold the graph arena (`Backend::graph_arena`).
pub const GRAPH_ARENA_REFUSAL: &str = "the admission arena's address-space reservation was refused (an address-space rail: RLIMIT_AS, or no overcommit)";

pub struct Backend {
    /// The level's fixed tables (`plans`, `owners`, `execs`: sized once at `sized_as`, kept with
    /// their capacity by `empty_deep`) and the admission root's `Rc` block (`admit_ctx`) on the per-backend holder (`BACKEND_OWNER`), charged once at construction (a fixed
    /// holder, D4537 R2). FIRST field: a door-1 charge drops before the bytes it counts are freed.
    _held: memledger::Charge,
    /// D2908: the entry's refusals, each plan's key with its reason (at most
    /// `OFF_REFUSALS`, the oldest given up first; gone with the plan's handle): read only while the memo still holds the
    /// entry's refusal of that key (`off_refusal`).
    /// On the per-backend holder as it grows (`BACKEND_OWNER`).
    off_refusals: memledger::ChargedVec<(PlanKey, String)>,
    pub entry: Entry,
    /// The fetch sites' census at the last served line (`fetch_site::FETCH_DIAG`): the served line prints the statement's delta.
    fetch_seen: [u64; 2],
    pub cache: ProgramCache,
    pub memo: PlanMemo,
    pub plans: Vec<Option<Served>>,
    /// D1789: the key each `plans` slot was filed for, kept while its plan is out for an execution (the slot then reads
    /// `None`): a filing takes only an unowned slot, so a checked-out plan's slot is never given to another key.
    owners: Vec<Option<PlanKey>>,
    pub execs: Vec<Option<Box<ExecSlot>>>,
    /// D3547: the exec slots (one bit each, `MAX_EXECS` <= 32) holding a live named portal's SELECT at this level: a
    /// statement started here sends them away first (`runner::send_away`), so a nested level can still FETCH them.
    pub named_home: u32,
    pub next_gen: u32,
    pub served: u64,
    pub fallthrough: u64,
    /// D3034 (f3): the running statement's narrowing `(granted width, width run)`, taken by its lease's report line at the
    /// statement's end (`GrantLease::drop`).
    pub narrow: Option<(u16, u16)>,
    /// Admissions run (decompose, facts, the shape walk): a memoized refusal adds none.
    pub admissions: u64,
    /// Admissions lowered again in plan order after a licensed form's Shape refusal (`runner::or_plan_order`).
    pub relowered: u64,
    /// D2845: the admission arena (`admit::ARENA_BYTES` of address space), mapped at the level's first admission
    /// (`graph_arena`), not when the level's state is built: a mapping an address-space rail refuses refuses that admission
    /// by name instead of a panic (XX000) before the statement reaches its grant gate, and the next admission maps again.
    graph_arena: Option<Arena>,
    /// D2845: the memory pool's hold on the mapped arena's resident prefix (inputs knobs grant.rs `admission_arena_kept`),
    /// given back with it.
    graph_kept: gravity_driver::Held,
    /// D4600: by `admit::lean::KeyFold` (a content key is a hash already; nothing iterates these maps).
    pub graph_words: HashMap<ContentKey, (Arc<admit::GraphTpl>, core::cell::Cell<u64>), admit::lean::KeyFold>,
    /// The recipes by content key (admit.rs): a hit opens no relation.
    /// Beside the recipes (D3438): this backend's host state of a program filed without one (`Program::hosted`).
    pub graph_registry: HashMap<ContentKey, (Arc<admit::RecipeSet>, Option<std::rc::Rc<dyn core::any::Any>>), admit::lean::KeyFold>,
    report: bool,
    victim: usize,
    /// D3881: shared with the admissions a freed deep level hands to the thread's pool (`Admitted::keep`), so their memory
    /// outlives the level.
    admit_ctx: std::rc::Rc<gravity_driver::mem::root::Root>,
    /// C3 S4c: what the level's refused admissions read from the catalog (`note_refused_reads`): the memo's refusals go
    /// once any of it moves (`follow_refusals`), so a statement a catalog change makes admissible admits again.
    refused_reads: (gravity_deps::Deps, core::cell::Cell<u64>),
    pub(crate) plan_scratch: crate::runner::PlanScratch,
}

/// The holder of a level's fixed tables and its kept refusals: the per-backend cache (a level is kept
/// for the session on its thread, a freed deep level's state shelved on the thread's pool, `admit::kept`), never a
/// session's owner, which a pooled thread's next session would find as a residue (D4531's
/// `KEPT_PROBE_OWNER`).
pub(crate) const BACKEND_OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PerBackend));

/// D2908: the refusals `Backend::off_refusals` keeps.
const OFF_REFUSALS: usize = 16;

/// The admissions' root (`GravityAdmit`, `Backend::admit_ctx`): it lives with the backend's state, so its bytes are the
/// backend's session's (LEDGER2: a Gravity-side root named at its creation); an admission's planning context is its child
/// (admit `plan::PlanCtx`), which names its statement's plan owner.
fn admit_root() -> std::rc::Rc<gravity_driver::mem::root::Root> {
    let root = gravity_driver::mem::root::Root::new("GravityAdmit");
    root.set_owner_session();
    std::rc::Rc::new(root) // alloc: Exec charged Backend::_held
}

/// C3 S4c: `Backend::refused_reads` with nothing read, its mark unchecked.
fn no_reads() -> (gravity_deps::Deps, core::cell::Cell<u64>) {
    (gravity_deps::Deps::default(), core::cell::Cell::new(gravity_deps::UNCHECKED))
}

impl Backend {
    #[doc(hidden)]
    pub fn new_for_test() -> Backend {
        Backend::new()
    }

    /// The state statement level `depth` builds (`admit::reentry::built`): a static level's, or past them (D3465) a deep
    /// one's (`DEEP_*`).
    #[doc(hidden)]
    pub fn for_level(depth: usize) -> Backend {
        if depth < crate::admit::reentry::MAX_DEPTH { Backend::new() } else { Backend::new_deep() }
    }

    fn new() -> Backend {
        let nplans = crate::inputs::knobs::seam::memo_slots(); // D1789: MAX_PLANS, fewer under an e2e's memo-slots knob
        Backend::sized(nplans, park_nodes(nplans, crate::inputs::knobs::diag::park_plans()), PlanMemo::new(), MAX_PLANS)
    }

    /// D3465: a deep statement level's state (`DEEP_*`). D3881: its knobs read once per thread (`deep_knobs`), as a deep
    /// recursion builds its levels at every call.
    fn new_deep() -> Backend {
        let (report, nplans) = deep_knobs();
        Backend::sized_as(nplans, DEEP_PARK_NODES, PlanMemo::with_sets(DEEP_MEMO_SETS), DEEP_PLANS, report)
    }

    fn sized(nplans: usize, park_nodes: usize, memo: PlanMemo, cached: usize) -> Backend {
        Backend::sized_as(nplans, park_nodes, memo, cached, crate::inputs::knobs::seam::report_on())
    }

    fn sized_as(nplans: usize, park_nodes: usize, memo: PlanMemo, cached: usize, report: bool) -> Backend {
        let mut plans = Vec::with_capacity(nplans); // alloc: Cold charged held
        plans.resize_with(nplans, || None); // alloc: Cold charged held
        let mut execs = Vec::with_capacity(MAX_EXECS); // alloc: Cold charged held
        execs.resize_with(MAX_EXECS, || None); // alloc: Cold charged held
        let owners: Vec<Option<PlanKey>> = vec![None; nplans]; // alloc: Cold charged held
        let admit_ctx = admit_root();
        // the three tables and the root's block are allocated (on this thread): their bytes move to the holder now, once
        let held = {
            use core::mem::size_of;
            let tables = plans.capacity() * size_of::<Option<Served>>() + owners.capacity() * size_of::<Option<PlanKey>>() + execs.capacity() * size_of::<Option<Box<ExecSlot>>>();
            let bytes = tables as u64 + memledger::arc_bytes::<gravity_driver::mem::root::Root>();
            memledger::Charge::open(BACKEND_OWNER, memledger::charge::release_of(BACKEND_OWNER), memledger::Kind::Region, memledger::charge::Door::Heap, bytes)
        };
        gravity_lowerv4::mechs_used::record(report); // the mechanisms line's record, on only where the line prints (per backend, as `report` below)
        // the seed once per backend (a process the postmaster did not draw for: single-user mode, a rig); a one-word read after the first
        gravity_hosts::dict::text_hosts::fp_seed();
        // PGRUST_GRAVITY_REPORT is read once per backend (a deep level's: once per thread); never per execution.
        Backend { _held: held, fetch_seen: [0; 2], entry: Entry::new(park_nodes), cache: ProgramCache::new(64 << 20, cached), memo, plans, owners, execs, named_home: 0, next_gen: 1, served: 0, fallthrough: 0, narrow: None, admissions: 0, relowered: 0, graph_arena: None, graph_kept: gravity_driver::Held::NONE, graph_words: HashMap::with_hasher(admit::lean::KeyFold::new()), graph_registry: HashMap::with_hasher(admit::lean::KeyFold::new()), report, victim: 0, admit_ctx, refused_reads: no_reads(), off_refusals: memledger::ChargedVec::new(Some(BACKEND_OWNER), memledger::Kind::Region), plan_scratch: crate::runner::PlanScratch::new() } // alloc: Cold
    }

    /// D2845: the admission arena's handle, mapped on first use; a refused mapping (an address-space rail: `RLIMIT_AS`, no
    /// overcommit) is the admission's refusal by name (0A000), never a panic.
    pub fn graph_arena(&mut self) -> Result<gravity_program::ArenaRef, admit::Refused> {
        if self.graph_arena.is_none() {
            self.graph_arena = Arena::try_new(admit::ARENA_BYTES);
            if self.graph_arena.is_some() {
                self.graph_kept = crate::inputs::knobs::grant::admission_arena_kept();
                ARENA_EXIT.with(|_| ()); // D656: armed once per thread; its Drop runs at the thread's end
            }
        }
        match &self.graph_arena {
            Some(arena) => Ok(arena.handle()),
            None => Err(admit::Refused { stage: "arena", what: GRAPH_ARENA_REFUSAL.into() }), // alloc: Cold
        }
    }

    /// D656: the admission arena unmapped and the pool's hold on it given back, the rest of the level left as it is (a
    /// kill-class exit's estate, `admit::reentry::release_thread_mappings`).
    /// D3881: the admission context, for the admissions a freed level hands to the thread's pool (`admit::kept`).
    pub(crate) fn admit_root(&self) -> std::rc::Rc<gravity_driver::mem::root::Root> {
        self.admit_ctx.clone()
    }

    /// D3881: a freed deep level's state emptied for the next deep level built (`admit::kept`'s shells), as `new_deep`
    /// builds it with its tables' allocations kept and its handle generations going on; its parked Nodes drop before its
    /// plans (D2190), its admission context is reset, or made anew while kept executions share it.
    pub(crate) fn empty_deep(&mut self) {
        let Backend { _held: _, entry, fetch_seen, cache, memo, plans, owners, execs, named_home, next_gen: _, served, fallthrough, narrow, admissions, relowered, graph_arena, graph_kept, graph_words, graph_registry, report: _, victim, admit_ctx, refused_reads, off_refusals, plan_scratch } = self;
        entry.empty();
        plan_scratch.drain();
        *fetch_seen = [0; 2];
        cache.reset();
        memo.clear();
        plans.iter_mut().for_each(|p| *p = None);
        owners.iter_mut().for_each(|o| *o = None);
        execs.iter_mut().for_each(|e| *e = None);
        *named_home = 0;
        (*served, *fallthrough, *admissions, *relowered, *victim) = (0, 0, 0, 0, 0);
        *narrow = None;
        *graph_kept = gravity_driver::Held::NONE;
        *graph_arena = None;
        graph_words.clear();
        graph_registry.clear();
        match std::rc::Rc::get_mut(admit_ctx) {
            Some(root) => root.reset(),
            None => *admit_ctx = admit_root(),
        }
        *refused_reads = no_reads();
        off_refusals.clear();
    }

    /// D3881: an emptied deep level's own bytes, an estimate: its state and its tables' slots.
    pub(crate) fn shell_bytes(&self) -> u64 {
        use core::mem::size_of;
        let tables = self.plans.capacity() * size_of::<Option<Served>>() + self.owners.capacity() * size_of::<Option<PlanKey>>() + self.execs.capacity() * size_of::<Option<Box<ExecSlot>>>() + self.memo.slot_bytes() + self.entry.park_slot_bytes();
        (size_of::<Backend>() + tables) as u64
    }

    pub(crate) fn release_graph_arena(&mut self) {
        self.graph_kept = gravity_driver::Held::NONE;
        self.graph_arena = None;
    }

    /// C3 S4c: a refused admission's catalog reads, held for the level's refusals (`follow_refusals`).
    pub(crate) fn note_refused_reads(&mut self, d: &gravity_deps::Deps) {
        if !d.is_empty() {
            self.refused_reads.0.absorb(d);
        }
    }

    /// C3 S4c (D2663's rule, by what was read): a catalog row a refused admission of this level read moved (a support proc,
    /// a function's volatility, an operator class): the memo's refusals go, so their keys admit again (reentry `settle`,
    /// at each borrow of the level). A served plan answers for itself (`catalog_stale`).
    pub fn follow_refusals(&mut self) {
        if self.refused_reads.0.is_empty() || self.refused_reads.0.current_since(&self.refused_reads.1) {
            return;
        }
        self.refused_reads = no_reads();
        self.memo.drop_refusals();
        self.off_refusals.clear();
    }

    /// C3 S4c: an admission whose catalog reads moved since it read them (`GraphWords::deps_hold`: D2663's support proc, a
    /// pg_proc row, an operator class; PostgreSQL's plancache keeps a generic plan across such a change, its executor looking
    /// the proc up at every start, while a filed program baked it) is never filed, served or given back; B2-31: nor one
    /// holding a constant converted in another TimeZone than the session's now (`Admitted::timezone`: PG's cross-type compare
    /// reads the session zone at every execution of a generic plan, a filed constant at its admission).
    pub fn catalog_stale(a: &Admitted) -> bool {
        gravity_decompose::normalize::zone_moved(a.timezone) || a.graph.as_ref().is_some_and(|g| !g.deps_hold())
    }

    /// Whether the plan memo may not serve admission `a` again (each serve it gives: bind's lookup, the seam's take, a
    /// rearm, a shelf revival): `catalog_stale`, or PS1-21 / PS1-26 step 1b, a program decision on a relation's pages that
    /// decides otherwise now (lower key.rs `rel_decide`: join.buildside builds a heap side its pages bound, and an INSERT
    /// grows the heap with no invalidation). One page count per such relation; the execution admits afresh. D3177 (the main
    /// merge's audit): or a domain constraint set the admission read is no longer its domain's (`DomainPins::current`).
    /// PGRCSERVE G5: or a road answer it was lowered under is another now (hosts am_rows.rs `Roads::hold`).
    pub fn serve_stale(a: &Admitted) -> bool {
        Backend::catalog_stale(a) || a.graph.as_ref().is_some_and(|g| !admit::rels_hold(g.rels()) || !g.domains.current()) || !a.roads.hold()
    }

    /// A served execution without a parallel census: counted always, logged only under PGRUST_GRAVITY_REPORT.
    pub fn note_served(&mut self, shape: &str, rows: u64, via: &'static str, code: &'static str) {
        self.note_served_at(shape, rows, 0, 0, 0, 0, [0; 5], [0; 3], (0, 0), (0, 0, [0; 4]), [0; 9], [0; 5], [0; 6], 0, 0, via, code, "")
    }

    /// A served execution that ran `sections` parallel sections at `width` through `claims` pool claims and `opens` part opens,
    /// its sources decoding / skipping / answering `granules` (a serial run says none); `code` = the first pipeline's code owner;
    /// `pipes` = the recorded run's ` pipes=.. lead_ms=..` (serial census M0; empty when nothing was recorded).
    pub fn note_served_at(&mut self, shape: &str, rows: u64, width: u16, sections: u32, claims: u64, opens: u64, granules: [u64; 5], first_touch: [u64; 3], cold: (u64, u64), p2: (u64, u64, [u32; 4]), io: [u64; 9], dict: [u64; 5], dictent: [u64; 6], elided: u64, node: u8, via: &'static str, code: &'static str, pipes: &str) {
        self.served += 1;
        if self.reporting() && self.served == 1 {
            let st = self.entry.stats();
            self.report_line(&format!("gravity: first serve of the session, phases us:{} (section open: provision={} bind={})", crate::phases::line(), st.provision_us, st.bind_us)); // alloc: Cold
        }
        // pglfast C2: what follows feeds the report's lines alone (`fetch_seen` is read only under it): a session that never
        // reports reads no census (a lock, a value flush and six loads a served statement, 0.1-0.3k instructions)
        if !self.report {
            return;
        }
        // the process-resident fingerprint tables' census (R2): hits, builds, resident tables and MiB, dropped with their part
        let (fh, fb, fr, fbytes, fret) = gravity_hosts::dict::text_hosts::fp_tables_resident();
        let fps = if self.reporting() && fb > 0 { format!(" fps=hits:{fh}/built:{fb}/resident:{fr}/{}MiB/retired:{fret}", fbytes >> 20) } else { String::new() }; // alloc: Cold
        // fetch = the by-rowid fetch sites' rows / (granule, column) decodes this statement (the fetch form's witness; none outside it)
        let fnow = [gravity_hosts::ops::scan::pgrc::fetch_site::FETCH_DIAG[0].load(core::sync::atomic::Ordering::Relaxed), gravity_hosts::ops::scan::pgrc::fetch_site::FETCH_DIAG[1].load(core::sync::atomic::Ordering::Relaxed)];
        let fetch = if self.reporting() && fnow != self.fetch_seen { format!(" fetch=rows:{}/decodes:{}", fnow[0].wrapping_sub(self.fetch_seen[0]), fnow[1].wrapping_sub(self.fetch_seen[1])) } else { String::new() }; // alloc: Cold
        self.fetch_seen = fnow;
        if self.reporting() && width > 0 {
            // the prime's census once it finished, and the process's first section's cost on the statement that paid it
            let primed = gravity_hosts::ops::scan::pgrc::prime::report().map_or(String::new(), |p| format!(" catalog={}/{}/{}/{}us sections={}/{}/{}B", p.relations, p.generations, p.parts, p.micros, p.complete, p.completed, p.section_bytes)); // alloc: Cold
            let cold = if cold != (0, 0) { format!(" cold_first_arm_us={} cold_first_section_us={}", cold.0, cold.1) } else { String::new() }; // alloc: Cold
            // first_touch = Stats sections read from part files / bodies served from the answers; why = reads per declared
            // reason (granule-walk, no-answers) — the build law's witness, gated by the cold probes and the e2e
            let d: [u32; 8] = core::array::from_fn(|i| gravity_driver::P2_DIAG[i].load(core::sync::atomic::Ordering::Relaxed));
            let p2fail = if p2.2 != [0; 4] { format!(" p2fail=part{}/cap{}/pbits{}/len{} diag={d:?}", p2.2[0], p2.2[1], p2.2[2], p2.2[3]) } else { String::new() }; // alloc: Cold
            // io = the part files' preads / bytes / ms inside preads (÷ the statement's ms = the mean read depth); runs = the
            // claim run reads (preads / bytes / hole bytes read and discarded / extents inserted / found resident); depth = the max in flight
            let io = format!(" io=preads:{}/{}KB/{}ms runs:{}/{}KB/holes:{}KB/extents:{}/resident:{} depth:{}", io[0], io[1] >> 10, io[2] / 1_000_000, io[3], io[4] >> 10, io[4].saturating_sub(io[7]) >> 10, io[5], io[6], io[8]); // alloc: Cold
            // dict = the dictionaries' faults: whole sections streamed (KB), block faults / their compressed KB, frame faults / KB
            let dict = if dict != [0; 5] { format!(" dict=whole:{}KB blocks:{}/{}KB frames:{}/{}KB", dict[0] >> 10, dict[1], dict[2] >> 10, dict[3], dict[4] >> 10) } else { String::new() }; // alloc: Cold
            // dictent = the dictionary entry census under PGRUST_PGRC_DICT_CENSUS=1 (dictresident.md §1.3)
            let dictent = if dictent != [0; 6] { format!(" dictent=entry:{}/materialized:{}/walked:{}/gathered:{}/decoded:{}/renders:{}", dictent[0], dictent[1], dictent[2], dictent[3], dictent[4], dictent[5]) } else { String::new() }; // alloc: Cold
            let elided = if elided != 0 { format!(" elided={elided}") } else { String::new() }; // alloc: Cold
            let node = if gravity_parallel::pool_count() > 1 { format!(" node={}/{}", node, gravity_parallel::pool_count()) } else { String::new() }; // alloc: Cold
            // B2: pool-thread catalog misses since this backend's last report (process-wide counter: a concurrent
            // session's may land here too); admission makes one a bug, so any is printed
            let misses = catalog_misses_since_last();
            let node = if misses != 0 { format!("{node} catalog_misses={misses}") } else { node }; // alloc: Cold
            let node = format!("{node}{}", staged_total()); // alloc: Cold
            self.report_line(&format!("gravity: served {shape} rows={rows} width={width} sections={sections} claims={claims} opens={opens} granules={}/{}/{}/{} parts_answered={} first_touch=sections:{}/answers:{} why=granule-walk:{}/no-answers:{} p2={}/{}{p2fail}{primed}{cold}{fps}{io}{dict}{dictent}{fetch}{elided}{node} admit={via} code={code}{pipes}", granules[0], granules[1], granules[2], granules[4], granules[3], first_touch[0] + first_touch[1], first_touch[2], first_touch[0], first_touch[1], p2.0, p2.1)); // alloc: Cold
            return;
        }
        if self.reporting() {
            self.report_line(&format!("gravity: served {shape} rows={rows}{fps}{fetch} admit={via} code={code}{pipes}{}", staged_total())); // alloc: Cold
        }
    }

    /// A served execution through another crossing than tcop's (`via=entry`; docs/gravity/entry.md §3).
    pub fn note_served_via(&mut self, shape: &str, rows: u64, admit: &'static str, code: &'static str, via: &'static str) {
        self.served += 1;
        if self.reporting() {
            self.report_line(&format!("gravity: served {shape} rows={rows} admit={admit} via={via} code={code}")); // alloc: Cold
        }
    }

    /// An EXPLAIN without ANALYZE of a plan with no program (entry explain.rs): its verdict, never a fallthrough (nothing runs).
    /// D3806: a statement the entry refuses on a road the row executor took (admit/entry `removed`, a cursor refusal)
    /// logs the named refusal line an admission's refusal logs, so e2e-refused-by-name.py finds its name. D4206: so does
    /// a run-ahead error's refusal that is the statement's (runner `note_tag_refused`).
    pub fn note_removed(&self, why: &str) {
        if self.reporting() {
            self.report_line(&format!("gravity: graph refused ({why}) total=0us")); // alloc: Cold
        }
    }

    pub fn note_explain_refused(&self, why: &str) {
        if self.reporting() {
            self.report_line(&format!("gravity: explain refused ({why}) via=entry:explain")); // alloc: Cold
        }
    }

    pub fn note_memo_hit(&self) {
        if self.reporting() {
            self.report_line("gravity: memo served plan (seam)");
        }
    }

    /// D1559 S2: a revival of a kept serve (entry shelf.rs) takes the memo's program in its kept shell: the memo hit's text
    /// then `kept serve`, so the memo census counts it and m6_f1_sqlfn_memo's S2 lines count the revivals alone.
    pub fn note_kept_serve(&self) {
        if self.reporting() {
            self.report_line("gravity: memo served plan (seam) kept serve");
        }
    }

    pub fn note_fallthrough(&mut self, why: &str) {
        self.fallthrough += 1;
        if self.reporting() {
            self.report_line(&format!("gravity: fallthrough ({why})")); // alloc: Cold
        }
    }

    /// The served program's footprint (`size_of::<Program>` + its arena) and shape, one LOG line under the report.
    pub fn note_program(&self, p: &gravity_program::Program) {
        if self.reporting() {
            self.report_line(&format!("gravity: program bytes={} sites={} lanes={} out={} pipelines={} slots={}", core::mem::size_of::<gravity_program::Program>() + p.arena_used(), p.sites.len(), p.sites.iter().map(|d| d.nlanes()).sum::<usize>(), p.out.len(), p.pipelines.len(), p.slots.nslots())); // alloc: Cold
        }
    }

    /// MECHCORPUS (main 09-29): the served program's content key and the kinds that made it (`Program::used`; `?` when
    /// lowered with the record off or by no v4 lowering), one LOG line under the report, read from the program (a cached
    /// one prints its own). A road's engagement shows in the key: under `gravity_road_off` a statement gets another program.
    pub fn note_mechs(&self, key: u128, used: u128) {
        if self.reporting() {
            let kinds = gravity_lowerv4::mechs_used::used_names(used);
            self.report_line(&format!("gravity: mechanisms key={key:032x} kinds={kinds}")); // alloc: Cold
        }
    }

    /// The report is on for this statement: PGRUST_GRAVITY_REPORT, and no keysem support call running (D3375 N7: a
    /// statement a comparator's or an equality's body runs, once per compared pair, builds no line, records no open
    /// probe; under the report d3032_reverse_opclass_walk's W0 run took 257 s against trunk's 121 s with the row
    /// executor answering those bodies).
    pub(crate) fn reporting(&self) -> bool {
        self.report && !gravity_fn::typesem::keysem::call::calling()
    }

    pub(crate) fn report_line(&self, msg: &str) {
        let fires = fires::drain(); // shapecheck: the optimizations that engaged since the last line (fires crate)
        // D3375 N7: a statement a comparator's or an equality's body runs, once per compared pair, writes no line (three
        // a call made d3032_reverse_opclass_walk's log 50 GB; the row executor ran those bodies with none before N7). The
        // counters still count it.
        if gravity_fn::typesem::keysem::call::calling() {
            return;
        }
        let fires = if fires.is_empty() { String::new() } else { format!(" fires={fires}") }; // alloc: Cold
        let line = elog::with_debug_query_string(|q| format!("{msg}{fires} served={} fallthrough={} :: {}", self.served, self.fallthrough, q.unwrap_or("").replace('\n', " "))); // alloc: Cold
        let _ = elog::ereport(types_error::LOG).errmsg(line).errhidestmt(true).errhidecontext(true).finish(types_error::ErrorLocation::new("gravity", 0, "report"));
    }

    pub fn lookup(&mut self, key: PlanKey) -> Option<Verdict> {
        self.memo.probe(key)
    }

    /// D1789: slot `idx` is filed for `key` and its plan is out for an execution (the give-back's slot).
    pub fn lent(&self, idx: usize, key: PlanKey) -> bool {
        self.plans.get(idx).is_some_and(Option::is_none) && self.owners.get(idx).is_some_and(|o| *o == Some(key))
    }

    pub fn admitted(&mut self, idx: usize, key: PlanKey) -> Option<&mut Admitted> {
        self.plans.get_mut(idx).and_then(Option::as_mut).filter(|s| s.key == key).map(|s| s.served.as_mut())
    }

    pub fn remember_refusal(&mut self, key: PlanKey) {
        let displaced = self.memo.insert(key, Verdict::Refused);
        self.release(displaced);
    }

    /// The entry's refusal of a keyed plan; never over a verdict bind wrote.
    pub fn remember_entry_refusal(&mut self, key: PlanKey) {
        if self.memo.probe(key).is_none() {
            let displaced = self.memo.insert(key, Verdict::EntryRefused);
            self.release(displaced);
        }
    }

    /// D2908: the entry's refusal of a keyed plan (`why`, the statement's error),
    /// memoized as the entry's refusal with its reason, so the plan's next execution raises it without lowering again
    /// (`off_refusal`); never over a verdict bind wrote (the reason is then not kept).
    pub fn remember_off_refusal(&mut self, key: PlanKey, why: &str) {
        self.remember_entry_refusal(key);
        self.off_refusals.with_vec(|v| v.retain(|(k, _)| *k != key));
        if self.memo.probe(key) != Some(Verdict::EntryRefused) {
            return;
        }
        if self.off_refusals.len() >= OFF_REFUSALS {
            self.off_refusals.with_vec(|v| drop(v.remove(0)));
        }
        // a refused growth keeps no reason: `off_refusal` then answers None, as for a reason never kept
        let _ = self.off_refusals.push((key, why.to_string())); // alloc: Cold
    }

    /// D2908: the reason `remember_off_refusal` kept for `key`, while the memo still holds the entry's refusal of it.
    pub fn off_refusal(&mut self, key: PlanKey) -> Option<&str> {
        if self.memo.probe(key) != Some(Verdict::EntryRefused) {
            return None;
        }
        self.off_refusals.iter().find(|(k, _)| *k == key).map(|(_, why)| why.as_str())
    }

    /// The plan filed under `key`: its slot, or `None` when every slot's plan is checked out (D1789: nothing is filed
    /// and the admission is dropped; the key admits again at its next execution), the plan holds the no-order-proc raise
    /// (W8-EQUIVCLASS-SORTOP, admit.rs `GraphTpl::no_proc`: never filed, so no take, rearm or shelf serves it again), or
    /// its catalog reads moved since its admission (D2663, `catalog_stale`). D4081: a key the memo already serves is filed
    /// into its own slot (a plan there is replaced, a plan out is dropped at its give-back), never a second one.
    pub fn remember_served(&mut self, key: PlanKey, served: Box<Admitted>) -> Option<usize> {
        if served.graph.as_ref().is_some_and(|g| g.no_proc) || Backend::catalog_stale(&served) {
            // D3346: not filed: dropped after the Nodes parked under its key (D2190)
            self.drop_admission(key, served);
            return None;
        }
        // D4081: a same-key insert displaces nothing (`PlanMemo::insert`), so filing the key elsewhere left its slot owned,
        // empty while its plan was out (a parked portal's serve) and named by no verdict: freed by nothing, every Bind
        // fell to MEMO_FULL once MAX_PLANS were. D3579: a slot a live portal runs keeps its plan (`reap` frees it)
        let own = match self.memo.peek(key) {
            Some(Verdict::Served(i)) if self.owners.get(i as usize).is_some_and(|o| *o == Some(key)) && !self.pinned(i as usize) => Some(i as usize),
            _ => None,
        };
        // D1789: an unowned slot (a checked-out plan's slot is empty but owned); full, the next victim holding its plan
        // (a checked-out one is never taken)
        let idx = match own.or_else(|| self.owners.iter().position(Option::is_none)) {
            Some(i) => i,
            None => {
                let n = self.plans.len();
                let from = self.victim % n;
                // D3579: never a plan a live portal runs (`pinned`)
                let i = (0..n).map(|k| (from + k) % n).find(|&i| self.plans[i].is_some() && !self.pinned(i))?;
                self.victim = i + 1;
                if let Some(v) = self.plans[i].take() {
                    // D4081: the victim's own verdict only, never one naming its key's other slot
                    if self.memo.peek(v.key) == Some(Verdict::Served(i as u16)) {
                        self.memo.remove(v.key);
                    }
                    self.forget(v.key.handle);
                }
                i
            }
        };
        // D2190: the plan replaced in the key's own slot drops after the Nodes parked under its key
        if let Some(old) = self.plans[idx].take() {
            self.drop_admission(old.key, old.served);
        }
        self.file(idx, key, served);
        self.owners[idx] = Some(key);
        let displaced = self.memo.insert(key, Verdict::Served(idx as u16));
        self.release(displaced);
        Some(idx)
    }

    /// An execution's plan back into the memo (`runner::give_back`, after its plan-identity check): into the slot it
    /// was lent from while the verdict still names it; dropped when another execution of the key already gave it back
    /// or a refusal displaced it; filed afresh when the verdict is gone or names a slot no longer the key's (D1789).
    pub fn file_back(&mut self, key: PlanKey, served: Box<Admitted>) {
        // D2663: its catalog reads moved: dropped with the Nodes parked under its key, as a failed run's admission, its
        // slot freed
        if Backend::catalog_stale(&served) {
            self.unlend(key);
            return self.drop_admission(key, served);
        }
        match self.lookup(key) {
            Some(Verdict::Served(i)) if self.lent(i as usize, key) => self.file(i as usize, key, served),
            // D2190: another execution of the key gave its plan back first: this one is dropped, so the Nodes parked
            // under the key (one may be this execution's, its recipes over this admission) go with it
            Some(Verdict::Served(i)) if self.plans.get(i as usize).and_then(Option::as_ref).is_some_and(|s| s.key == key) => self.drop_admission(key, served),
            Some(Verdict::Served(_)) | None => {
                let _ = self.remember_served(key, served);
            }
            // D3346: a refusal displaced the key: dropped after the Nodes parked under it (D2190)
            _ => self.drop_admission(key, served),
        }
    }

    /// D3547: a named portal's admission lent away (`runner::send_away`), back at its PortalDrop: into the slot it was
    /// lent from while the memo still names that slot for the key (the lent state, D1789); else dropped after the Nodes
    /// parked under its key (D2190), never filed afresh: the verdict went while the portal held it (an invalidation, a
    /// plan-cache reset, a displacing verdict), so a later execution admits again, as D3579's `reap` drops such a plan.
    pub fn give_back_away(&mut self, key: PlanKey, served: Box<Admitted>) {
        match self.lookup(key) {
            Some(Verdict::Served(i)) if self.lent(i as usize, key) && !Backend::catalog_stale(&served) => self.file(i as usize, key, served),
            _ => {
                self.unlend(key);
                self.drop_admission(key, served)
            }
        }
    }

    /// D4844: `served` into plan slot `i` for `key`, every filing's one road (`remember_served`, `file_back`,
    /// `give_back_away`): its words' bound sets (admit `bind_words`, `WordTpl::Set`, built at the admission under the Bind's
    /// portal, a lent key's second admission under its own, on that portal's charge) become the session's, as the memo
    /// holds them past that portal's end (a named statement's Close, a later Bind's hit). A plan the memo does not file
    /// keeps them on the portal and drops them with it.
    fn file(&mut self, i: usize, key: PlanKey, mut served: Box<Admitted>) {
        if let Some(g) = served.graph.as_mut() {
            g.adopt_sets();
        }
        self.plans[i] = Some(Served { key, served });
    }

    /// C3 S4c: `key`'s slot, lent to the execution giving back a plan the memo no longer takes (its catalog reads moved),
    /// freed with its verdict; nothing when the slot is not lent for `key`.
    fn unlend(&mut self, key: PlanKey) {
        if let Some(Verdict::Served(i)) = self.memo.probe(key) {
            if self.lent(i as usize, key) {
                self.memo.remove(key);
                self.owners[i as usize] = None;
            }
        }
    }

    /// D2190: an execution's admission dropped rather than filed back (its run failed, or another execution of the key
    /// filed first). A Node parked under the key may have been built over it (a recipe reading its parameter copy), so
    /// the key's parked Nodes drop too: the next execution builds its Node over its own admission.
    pub fn drop_admission(&mut self, key: PlanKey, served: Box<Admitted>) {
        self.entry.park().drop_for(key.handle);
        drop(served);
    }

    fn release(&mut self, displaced: Option<(PlanKey, Verdict)>) {
        if let Some((key, Verdict::Served(i))) = displaced {
            if self.owners.get(i as usize).is_some_and(|o| *o == Some(key)) {
                // D1789: a checked-out plan's slot is freed too (its give-back finds another verdict and drops it).
                // D3351: the Nodes parked under the key drop first (D2190). D3579: a plan a live portal runs stays (`orphan`)
                self.forget(key.handle);
                if !self.orphan(i as usize) {
                    self.plans[i as usize] = None;
                    self.owners[i as usize] = None;
                }
            }
        }
    }

    fn forget(&mut self, h: Handle) {
        self.entry.park().drop_for(h);
        self.cache.drop_for(h);
        self.off_refusals.with_vec(|v| v.retain(|(k, _)| k.handle != h));
    }

    /// D3351: the parked Nodes drop before the admissions they may be built over (D2190), then the slots.
    pub fn drop_plans(&mut self, handle: Option<Handle>) {
        match handle {
            Some(h) => {
                self.memo.drop_for(h);
                self.forget(h);
            }
            None => {
                self.memo.clear();
                self.off_refusals.clear();
                self.entry.park().clear();
                self.cache.reset();
                self.graph_words.clear();
                self.graph_registry.clear();
            }
        }
        for i in 0..self.plans.len() {
            // D3579: a plan a live portal runs stays (`orphan`)
            if self.owners[i].is_some_and(|k| handle.is_none_or(|h| k.handle == h)) && !self.orphan(i) {
                self.plans[i] = None;
                self.owners[i] = None;
            }
        }
    }

    /// D3579: plan slot `i` holds the plan a live portal runs (its exec slot names the slot and the plan's key since Bind).
    /// PostgreSQL's portal holds its CachedPlan to PortalDrop, so no memo change frees that plan: not an invalidation, a
    /// plan-cache reset (DISCARD PLANS, a schema or function change), the plansource's drop (a simple Query replacing the
    /// unnamed statement, a Close), a displacing verdict, an eviction or a lending (`take_served`).
    pub(crate) fn pinned(&self, i: usize) -> bool {
        let Some(key) = self.plans.get(i).and_then(Option::as_ref).map(|s| s.key) else { return false };
        self.execs.iter().flatten().any(|s| s.runs(i, key))
    }

    /// D3579: plan slot `i` would be freed (its verdict gone): when a live portal runs its plan (`pinned`), the plan stays,
    /// its slot still owned so no filing takes it, and its portals no longer park Nodes under its key (`ExecSlot::orphan`);
    /// the last one's PortalDrop drops it (`reap`). `true` when kept.
    fn orphan(&mut self, i: usize) -> bool {
        if !self.pinned(i) {
            return false;
        }
        let key = self.plans[i].as_ref().map(|s| s.key).expect("pinned");
        self.execs.iter_mut().flatten().filter(|s| s.runs(i, key)).for_each(|s| s.orphan());
        true
    }

    /// D3579: a portal that ran plan slot `i` (`key`) was dropped: the plan goes when the memo no longer names it there
    /// (`orphan` kept it) and no other live portal runs it, after the Nodes parked under its key (D2190).
    pub(crate) fn reap(&mut self, i: usize, key: PlanKey) {
        let kept = self.plans.get(i).and_then(Option::as_ref).is_some_and(|s| s.key == key) && self.owners[i] == Some(key);
        if !kept || self.memo.probe(key) == Some(Verdict::Served(i as u16)) || self.pinned(i) {
            return;
        }
        self.owners[i] = None;
        if let Some(s) = self.plans[i].take() {
            self.drop_admission(key, s.served);
        }
    }
}

leader_tls::leader_tls! {
    thread_local! {
        static SESSION_END: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// D3881: a deep level's knobs (`Backend::new_deep`): the report knob and its memo slots, read at the thread's first.
        static DEEP_KNOBS: core::cell::Cell<Option<(bool, usize)>> = const { core::cell::Cell::new(None) };
        /// B2 (workersess): the pool catalog miss count at this backend's last report line (`catalog_misses_since_last`).
        static LAST_MISSES: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
        /// D656: the thread-end guard of the admission arenas (`graph_arena` arms it).
        static ARENA_EXIT: ArenaExit = const { ArenaExit };
    }
}

/// D3881: `DEEP_KNOBS`, read now at the thread's first deep level.
fn deep_knobs() -> (bool, usize) {
    DEEP_KNOBS.with(|k| {
        k.get().unwrap_or_else(|| {
            let v = (crate::inputs::knobs::seam::report_on(), crate::inputs::knobs::seam::memo_slots().min(DEEP_PLANS));
            k.set(Some(v));
            v
        })
    })
}

/// D3881: the report knob a deep level is built with (`admit::reentry::built` times only a reported build).
pub(crate) fn deep_report() -> bool {
    deep_knobs().0
}

/// D656: a backend thread whose session never ran its teardown (a crash or a kill: launch_backend's `run_child_task` skips the
/// session drain on kill-class exits, as C's `_exit` leaves the estate to the process's death) ends with its levels still
/// built, and the reentry levels are `ManuallyDrop`. The process outlives the crash cycle in the thread model, so every
/// crash-restart kept each killed backend's admission arenas (`admit::ARENA_BYTES` of address space a level, up to
/// `arena::KEEP_BYTES` of it resident) and the memory pool's hold on each (`admission_arena_kept`). At the thread's end the
/// arenas go and the holds come back; nothing else of the levels is touched (their contexts, parked Nodes and pins belong to
/// the crash cycle). After a clean session end the levels are gone already and this finds nothing.
struct ArenaExit;

impl Drop for ArenaExit {
    fn drop(&mut self) {
        crate::admit::reentry::release_thread_mappings();
    }
}

pub fn ensure_session_end() {
    if !SESSION_END.replace(true) {
        // D2373: the backend levels (their admit contexts) go in the State phase, after every Portals cleanup: the entry's
        // parked descs (released with their portal shells) and shelved serves (shelf `drain`) hold admissions allocated
        // there, and a Portals registration order (LIFO) put this teardown first whenever a session's first desc
        // registered before its first build
        mcx::register_session_cleanup_phase(mcx::SessionCleanupPhase::State, Box::new(invalidate::session_end)); // alloc: Cold
    }
}

/// The backend state of the first level no running statement holds (reentry.rs, built on demand).
pub fn with_backend<R>(f: impl FnOnce(&mut Backend) -> R) -> Option<R> {
    crate::admit::reentry::enter(Some(crate::admit::reentry::built), None, false, |b, _| f(b)).ok().flatten()
}

pub fn with_existing_backend<R>(f: impl FnOnce(&mut Backend) -> R) -> Option<R> {
    crate::admit::reentry::enter(None, None, false, |b, _| f(b)).ok().flatten()
}

pub fn teardown() {
    SESSION_END.set(false);
    crate::admit::reentry::teardown();
}

pub fn flush_later() {
    crate::admit::reentry::flush_later();
}

/// B2: catcache_seams' pool catalog misses since this thread's previous call.
fn catalog_misses_since_last() -> u64 {
    let now = catcache_seams::pool_catalog_misses();
    now - LAST_MISSES.with(|l| l.replace(now)).min(now)
}

/// D3168 S0: the served line's ` staged_total=made:<n>/compiled:<c>/<us>us/skipped:<s>/wanted:<w>`: the staged functions made in the process so far,
/// those of them that compiled (a code cache miss) and those compiles' time (gravity_program `STAGED_MADE`; process-wide
/// totals: a census differences two statements' lines); nothing until one was made.
fn staged_total() -> String {
    use core::sync::atomic::Ordering::Relaxed;
    let (n, skipped) = (gravity_program::code::STAGED_MADE.load(Relaxed), gravity_program::code::STAGED_SKIPPED.load(Relaxed));
    if n == 0 && skipped == 0 {
        return String::new();
    }
    let (c, ns, wanted) = (gravity_program::code::STAGED_COMPILED.load(Relaxed), gravity_program::code::STAGED_NS.load(Relaxed), gravity_program::code::STAGED_WANTED.load(Relaxed));
    format!(" staged_total=made:{n}/compiled:{c}/{}us/skipped:{skipped}/wanted:{wanted}", ns / 1000) // alloc: Cold
}

/// B2 (workersess): a section's catalog arm: the images of the types its program's catalog-armed hosted sites reach
/// (lowerv4 `catalog_reach`), built on the leader and made the leader's statement arm for the section, so each claim's
/// carry arms them (gravity_fn pool_catalog). A build that no longer images (a type changed under the cached program)
/// arms nothing: a worker's read then misses by name (counted), never a wrong answer.
fn section_catalog(p: &gravity_program::Program) -> Option<Box<dyn core::any::Any>> {
    let reach = match (p.owner.as_deref(), p.hosted.as_deref()) {
        (Some(o), _) => gravity_lowerv4::hir::catalog_reach(o)?,
        // D3438: a program filed without its owner reaches what its template's lowering found
        (None, Some(t)) => gravity_lowerv4::hir::template_reach(t)?,
        (None, None) => return None,
    };
    if reach.is_empty() {
        return None;
    }
    let cat = gravity_fn::pool_catalog::StatementCatalog::build(reach).ok().flatten()?;
    Some(Box::new(gravity_fn::pool_catalog::arm_leader_statement(Some(std::sync::Arc::new(cat))))) // alloc: Exec
}

pub fn init_seams() {
    // D3406: a kernel's set byte runs the poll (the owner's CHECK_FOR_INTERRUPTS on its service bit)
    gravity_abi::ctx::set_poll(glue::poll_kernel);
    gravity_parallel::install_section_arm(section_catalog);
    gravity_hosts::ops::scan::bitmap_image::install_fan_out(leaf_fan_out);
    gravity_kernels::table::fold_jit::install(crate::table::fold_code::compile);
    // D4635: a dropped relation's or database's page hints leave the process-wide table with its buffers
    gravity_hosts::ops::scan::page_hints::install_forget();
    crate::inputs::knobs::lowerer::install_lowerer_check();
    // D3793: a reload (or ALTER SYSTEM) to a part cache setting past what the memory limit leaves is refused
    gravity_hosts::memory_budget::install_part_cache_check();
    crate::inputs::knobs::dump::install_dump_hooks();
    crate::inputs::knobs::perfmap::install_perfmap_hooks();
    crate::inputs::pgrc::install_condition_cache_derivation();
    // D3402: keysem's bound-call table binds a SQL comparator whose body is one quiet built-in call as that built-in
    gravity_fn::typesem::keysem::call::set_alias_verdict(gravity_hosts::facts::alias_verdict);
    // D2605: each served ModifyTable's AFTER STATEMENT events queue at its own end (driver node/ends.rs)
    crate::ops::modifytable::after_query::stmt_ends_registered();
    // Gravity's correctness rides the plan-key generation (a replanned plan re-admits). D3059: plancache's drop / reset
    // channels free what a dropped plansource (DEALLOCATE, a prepared statement's teardown) or a reset (DISCARD PLANS / ALL,
    // ResetPlanCache) left: each idle level's owner slot, filed plan and memo entries, and on a reset the parked nodes, the
    // program cache and the graph words (invalidate.rs). SQE's statement memo held these slots until its deletion.
    plancache_portal_seams::engine_portal_dropped::set(runner::drop_by_token);
    plancache_portal_seams::engine_plan_dropped::set(invalidate::engine_plan_dropped);
    plancache_portal_seams::engine_plan_cache_reset::set(invalidate::engine_plan_cache_reset);
    // D4579: DROP DATABASE: the process-wide program tier lets go the programs lowered in the dropped database
    l2cache::set_forget_database_hook(admit::forget_database);
    // the tcop entries (gravity_entry_seams): tcop no longer links gravity_exec directly
    gravity_entry_seams::bind::set(runner::bind);
    gravity_entry_seams::describe::set(runner::describe);
    gravity_entry_seams::execute::set(runner::execute);
    gravity_entry_seams::fetch::set(runner::fetch);
    gravity_entry_seams::simple::set(runner::simple);
    gravity_entry_seams::simple_refused_at_admission::set(runner::simple_refused_at_admission);
    gravity_entry_seams::note_replanned::set(runner::note_replanned);
    gravity_entry_seams::replan_arm::set(runner::replan_arm);
    gravity_entry_seams::replan_end::set(runner::replan_end);
    gravity_entry_seams::plan_skip::set(runner::plan_skip);
    gravity_entry_seams::feedback_flush::set(inputs::knobs::feedback_file::flush);
    // the process's fingerprint seed: drawn by the postmaster (`launch_backend::rtpool::gravity_prewarm_register`), else by
    // the backend at `Backend::new`; here only when a rig set the directory
    gravity_hosts::dict::text_hosts::fp_seed_boot();
}

/// D1789: the plan memo's slot owners (a checked-out plan's slot is never given to another key).
#[cfg(test)]
#[allow(unsafe_code)]
mod d1789_slot_owners {
    use super::*;
    use gravity_program::{Code, End, Generation, Handle, Layout, Pipeline, Reason, WritePlan};

    unsafe extern "C-unwind" fn nop(_: *mut gravity_abi::Ctx) -> i64 {
        0
    }

    fn key(handle: u64) -> PlanKey {
        PlanKey { handle: Handle(handle), generation: Generation(7) }
    }

    /// A graph admission's words with nothing bound, holding `deps` as its recorded catalog reads.
    fn words(no_proc: bool, deps: gravity_deps::Deps) -> admit::GraphWords {
        admit::GraphWords { words: Box::new([]), key: gravity_program::ContentKey(0), inits: Vec::new(), arms: Vec::new(), prune: None, tpl: None, sets: Vec::new(), held: Default::default(), no_proc, est: None, partial: false, rels_decide: false, folded: Box::new([]), genp: Default::default(), domains: Default::default(), deferred: Box::new([]), reads: gravity_deps::Held::of(deps) } // alloc: Cold
    }

    /// stmtoh F6: a static level parks as many Nodes as it keeps plans, never fewer than `PARK_NODES`; off, `PARK_NODES`.
    #[test]
    fn a_static_level_parks_as_many_nodes_as_it_keeps_plans() {
        assert_eq!(park_nodes(MAX_PLANS, true), MAX_PLANS);
        assert_eq!(park_nodes(4, true), PARK_NODES, "an e2e's small memo keeps the old park");
        assert_eq!(park_nodes(MAX_PLANS, false), PARK_NODES, "off: the old park");
        assert!(MAX_PLANS >= PARK_NODES);
    }

    /// The keys 0..MAX_PLANS never displace one another's verdict in the set-associative memo (the tests count slots).
    #[test]
    fn the_test_keys_fit_the_memo_sets() {
        let mut per = std::collections::HashMap::<usize, usize>::new();
        for h in 0..MAX_PLANS as u64 + 1 {
            *per.entry(crate::admit::memo::set_index(key(h))).or_default() += 1;
        }
        let mut k5000 = per.clone();
        *k5000.entry(crate::admit::memo::set_index(key(5000))).or_default() += 1;
        assert!(k5000.values().all(|&n| n <= crate::admit::memo::WAYS));
    }

    fn program() -> Arc<Program> {
        let layout = Layout { nsites: 0, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
        Arc::new(Program::with_pipelines(layout, &[], &[], WritePlan::SELECT, [(Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(nop))])) // alloc: Cold
    }

    fn admitted(p: &Arc<Program>) -> Box<Admitted> {
        let key_exprs = shapes::KeyExprs { stable: Vec::new().into_boxed_slice(), exprs: Vec::new(), acl_funcs: Vec::new(), filters: 0, rechecks: 0 }; // alloc: Cold
        Box::new(Admitted { shape: "d1789", program: p.clone(), registry: shapes::empty_registry(), key_exprs, nkparams: 0, keys: [shapes::KeySrc::Const(0); shapes::SHAPE_PARAMS], cmd: gravity_program::Cmd::Select, out_natts: 0, eval_at_execute: false, sort_keys: [0; 4], nsort: 0, walk: None, pstmt: None, graph: None, params: types_portal::ParamListHandle::NULL, query_env: types_portal::QueryEnvHandle::NULL, timezone: None, memo: Default::default(), keep: None, ident: None, roads: Default::default() }) // alloc: Cold
    }

    /// A level's fixed tables are the per-backend holder's from construction (one fixed charge of
    /// their capacities), kept across `empty_deep` (the tables keep their allocations), given back at the drop; its kept
    /// refusals are the holder's as they grow.
    #[test]
    fn e_hold_a_levels_tables_are_the_per_backend_holders() {
        use core::mem::size_of;
        let mut b = Backend::new_for_test();
        let tables = b.plans.capacity() * size_of::<Option<Served>>() + b.owners.capacity() * size_of::<Option<PlanKey>>() + b.execs.capacity() * size_of::<Option<Box<ExecSlot>>>();
        assert!(tables > 0);
        let held = tables as u64 + memledger::arc_bytes::<gravity_driver::mem::root::Root>();
        assert_eq!((b._held.owner(), b._held.committed()), (BACKEND_OWNER, held), "the three tables and the admission root's block, charged once");
        assert_eq!(b.off_refusals.owner(), Some(BACKEND_OWNER));
        b.empty_deep();
        assert_eq!(b._held.committed(), held, "an emptied deep level keeps its tables and their charge");
    }

    /// Condition (1): every slot's plan checked out, a filing files nothing (no growth, no slot taken) and drops its plan.
    #[test]
    fn every_slot_checked_out_files_nothing() {
        let mut b = Backend::new_for_test();
        let p = program();
        let mut out = Vec::new(); // alloc: Cold
        for h in 0..MAX_PLANS as u64 {
            let i = b.remember_served(key(h), admitted(&p)).expect("a free slot");
            out.push((i, key(h), b.plans[i].take().expect("filed").served)); // checked out, as `take_served`
        }
        assert_eq!(Arc::strong_count(&p), 1 + MAX_PLANS);
        assert_eq!(b.remember_served(key(9999), admitted(&p)), None, "no slot is free and none may be taken");
        assert_eq!(b.lookup(key(9999)), None);
        assert_eq!(Arc::strong_count(&p), 1 + MAX_PLANS, "the unfiled plan is dropped, nothing else");
        assert_eq!(b.plans.len(), MAX_PLANS);
        for (i, k, a) in out {
            assert!(b.lent(i, k), "slot {i} still lent to its key");
            b.file_back(k, a);
            assert!(b.plans[i].as_ref().is_some_and(|s| s.key == k), "slot {i} back with its key's plan");
        }
        assert_eq!(Arc::strong_count(&p), 1 + MAX_PLANS);
        b.drop_plans(None);
        assert_eq!(Arc::strong_count(&p), 1);
    }

    /// Condition (2): an eviction frees the victim's plan exactly once; a checked-out slot is skipped, and its key's
    /// later give-back (or its verdict's release while out) neither frees twice nor leaks.
    #[test]
    fn an_eviction_frees_once_and_skips_a_checked_out_slot() {
        let mut b = Backend::new_for_test();
        let p = program();
        for h in 0..MAX_PLANS as u64 {
            assert_eq!(b.remember_served(key(h), admitted(&p)), Some(h as usize));
        }
        let held = b.plans[0].take().expect("filed").served; // key 0 checked out: slot 0 empty, still owned
        assert_eq!(Arc::strong_count(&p), 1 + MAX_PLANS);
        let i = b.remember_served(key(5000), admitted(&p)).expect("a victim holding its plan");
        assert_eq!(i, 1, "the checked-out slot 0 is skipped");
        assert_eq!(b.lookup(key(1)), None, "the victim's verdict goes with it");
        assert_eq!(Arc::strong_count(&p), 1 + MAX_PLANS, "one plan in, the victim's out: freed once");
        b.file_back(key(0), held);
        assert!(b.plans[0].as_ref().is_some_and(|s| s.key == key(0)), "key 0's plan back in its own slot");
        assert_eq!(Arc::strong_count(&p), 1 + MAX_PLANS);
        // a verdict released while its plan is out: the slot is freed, the give-back files the plan afresh, once
        let held = b.plans[2].take().expect("filed").served;
        b.drop_plans(Some(Handle(2)));
        assert_eq!(b.lookup(key(2)), None);
        b.file_back(key(2), held);
        assert_eq!(b.plans.iter().flatten().filter(|s| s.key == key(2)).count(), 1, "filed once");
        assert_eq!(Arc::strong_count(&p), 1 + MAX_PLANS);
        // a second give-back of the key (another execution's) is dropped, never filed twice
        b.file_back(key(2), admitted(&p));
        assert_eq!(b.plans.iter().flatten().filter(|s| s.key == key(2)).count(), 1);
        assert_eq!(Arc::strong_count(&p), 1 + MAX_PLANS);
        b.drop_plans(None);
        assert_eq!(Arc::strong_count(&p), 1, "no plan leaked");
    }

    /// D4081: a key filed again while its plan is out (a Bind's admission while a parked portal's serve holds the plan)
    /// goes into its own slot, and the plan out is dropped at its give-back: one slot owned whatever the cycles, so
    /// MAX_PLANS + 1 of them leave the memo room for another key (before, each left one slot owned, empty and unnamed).
    #[test]
    fn a_key_filed_again_while_its_plan_is_out_keeps_one_slot() {
        let mut b = Backend::new_for_test();
        let p = program();
        let i = b.remember_served(key(1), admitted(&p)).expect("filed");
        for round in 0..=MAX_PLANS {
            let out = b.plans[i].take().expect("filed").served; // checked out, as `take_served`
            assert_eq!(b.remember_served(key(1), admitted(&p)), Some(i), "round {round}: the key's own slot");
            b.file_back(key(1), out);
            assert!(b.plans[i].as_ref().is_some_and(|s| s.key == key(1)), "round {round}: the slot keeps the newer plan");
            assert_eq!(b.owners.iter().flatten().count(), 1, "round {round}: one slot owned");
            assert_eq!(Arc::strong_count(&p), 2, "round {round}: the plan given back is dropped");
        }
        assert_eq!(b.remember_served(key(1), admitted(&p)), Some(i), "a plan in the slot is replaced in place");
        assert_eq!(Arc::strong_count(&p), 2);
        assert!(b.remember_served(key(2), admitted(&p)).is_some_and(|j| j != i), "the memo has room for another key");
        b.drop_plans(None);
        assert_eq!(Arc::strong_count(&p), 1, "no plan leaked");
    }

    /// W8-EQUIVCLASS-SORTOP (code review r1 S1): an admission holding the no-order-proc raise is never filed (no verdict,
    /// no slot, its plan dropped), so its key admits again at every execution; any other admission is filed as before.
    #[test]
    fn a_no_order_proc_plan_is_never_filed() {
        let mut b = Backend::new_for_test();
        let p = program();
        let mut a = admitted(&p);
        a.graph = Some(words(true, gravity_deps::Deps::default()));
        assert_eq!(b.remember_served(key(1), a), None);
        assert_eq!(b.lookup(key(1)), None);
        assert_eq!(Arc::strong_count(&p), 1, "the unfiled plan is dropped");
        assert_eq!(b.remember_served(key(1), admitted(&p)), Some(0), "does not apply: a plan without the raise is filed");
        b.drop_plans(None);
        assert_eq!(Arc::strong_count(&p), 1);
    }

    /// C3 S4c (D2663's rule, by what was read): an admission whose recorded catalog reads moved (a support proc, a pg_proc
    /// row, an operator class) is never served again, a plan out for an execution across the move is dropped at its
    /// give-back (its slot freed), and an admission that read before the move is never filed; one whose reads hold files
    /// as before, and a filed plan that read nothing that moved stays. The level's refusals go once a read of a refused
    /// admission moves (`follow_refusals`), and only then.
    #[test]
    fn an_admission_whose_catalog_reads_moved_is_never_filed_or_served() {
        let (r, other): (u32, u32) = (0xFFFF_E3C1, 0xFFFF_E3C2);
        let read = |rel: u32| {
            let ((), d) = gravity_deps::recording(|| gravity_deps::note_rel(rel));
            d
        };
        let reading = |p: &Arc<Program>, rel: u32| {
            let mut a = admitted(p);
            a.graph = Some(words(false, read(rel)));
            a
        };
        let mut b = Backend::new_for_test();
        let p = program();
        let i = b.remember_served(key(1), reading(&p, r)).expect("filed");
        let out = b.plans[i].take().expect("filed").served; // checked out, as `take_served`
        let j = b.remember_served(key(2), reading(&p, r)).expect("filed");
        let k = b.remember_served(key(3), reading(&p, other)).expect("filed");
        let old = reading(&p, r); // an admission under way across the change
        b.remember_refusal(key(5));
        b.note_refused_reads(&read(r));
        b.follow_refusals();
        assert!(b.admitted(j, key(2)).is_some_and(|a| !Backend::serve_stale(a)), "no change: served");
        assert_eq!(b.lookup(key(5)), Some(Verdict::Refused), "no change: the refusal stays");
        l2cache::invalidate_key(l2cache::Domain::Rel(r), l2cache::rel_key(r, 0));
        assert!(b.admitted(j, key(2)).is_some_and(|a| Backend::serve_stale(a)), "its read moved: never served again");
        assert!(b.admitted(k, key(3)).is_some_and(|a| !Backend::serve_stale(a)), "a plan that read nothing that moved stays served");
        b.file_back(key(1), out);
        assert_eq!(b.lookup(key(1)), None, "the plan out for an execution is dropped, not filed back");
        assert!(b.owners[i].is_none(), "and its slot is free");
        assert_eq!(b.remember_served(key(4), old), None, "an admission that read before the change is not filed");
        b.follow_refusals();
        assert_eq!(b.lookup(key(5)), None, "a refused admission's read moved: the refusal goes");
        assert_eq!(b.lookup(key(3)), Some(Verdict::Served(k as u16)), "a filed plan answers for itself");
        let n = b.remember_served(key(6), reading(&p, r)).expect("an admission that read after the change is filed");
        assert!(b.admitted(n, key(6)).is_some_and(|a| !Backend::serve_stale(a)));
        b.drop_plans(None);
        assert_eq!(Arc::strong_count(&p), 1);
    }

    /// D3351 (D2190's order): a Node parked under a key may be built over the key's filed admission (its recipes read that
    /// admission's parameter copy), so each path that drops the filed plan drops the key's parked Nodes FIRST: an
    /// invalidation flush (`drop_plans(Some(h))`), the session's end (`drop_plans(None)`) and a displaced verdict's release.
    /// The probe is a recipe in the admission's registry: when the admission drops, it records whether the Node parked
    /// under its key (built over its own program, which only that Node holds) is still alive.
    #[test]
    fn the_parked_nodes_drop_before_the_admission_they_may_be_built_over() {
        struct Probe {
            seen: std::rc::Rc<std::cell::Cell<Option<bool>>>,
            node_program: std::sync::Weak<Program>,
        }
        impl gravity_hosts::recipe::HostRecipe for Probe {
            fn rel(&self) -> types_core::Oid {
                0
            }
            fn size_hint(&self) -> usize {
                0
            }
            fn derive(&self, _: &gravity_program::SiteDesc, _: gravity_driver::ArenaRef) -> gravity_driver::SourceBox {
                unreachable!("the probe's program has no site")
            }
            fn clone_box(&self) -> Box<dyn gravity_hosts::recipe::HostRecipe> {
                Box::new(Probe { seen: std::rc::Rc::new(std::cell::Cell::new(None)), node_program: self.node_program.clone() }) // alloc: Cold
            }
        }
        impl Drop for Probe {
            fn drop(&mut self) {
                self.seen.set(Some(self.node_program.upgrade().is_some()));
            }
        }
        for path in ["drop_plans(Some)", "drop_plans(None)", "release"] {
            let mut b = Backend::new_for_test();
            let p = program();
            let pn = program();
            let seen = std::rc::Rc::new(std::cell::Cell::new(None)); // alloc: Cold
            let mut a = admitted(&p);
            a.registry.push_rung(Box::new(Probe { seen: seen.clone(), node_program: Arc::downgrade(&pn) })); // alloc: Cold
            let i = b.remember_served(key(1), a).expect("filed");
            b.entry.park().put(key(1), Box::new(gravity_driver::Node::build(pn, &mut shapes::empty_registry()))); // alloc: Cold
            assert_eq!(b.entry.park().len(), 1);
            match path {
                "drop_plans(Some)" => b.drop_plans(Some(Handle(1))),
                "drop_plans(None)" => b.drop_plans(None),
                _ => b.release(Some((key(1), Verdict::Served(i as u16)))),
            }
            assert_eq!(seen.get(), Some(false), "{path}: the Node parked under the key drops before the admission");
            assert!(b.entry.park().is_empty(), "{path}: no Node left parked under the key");
            assert_eq!(Arc::strong_count(&p), 1, "{path}: the admission dropped");
        }
    }
}

/// A2 entry 2: hosts' parallel btree leaf walk (bitmap_image.rs `getbitmap_wide`) over the installed pool, one claim per
/// thread (each job walks the shared scan until it is done); the caller parks until every claim ran. `Ok(false)`: no pool.
fn leaf_fan_out(width: u16, job: &gravity_hosts::ops::scan::bitmap_image::LeafJob<'_>) -> types_error::PgResult<bool> {
    let Some(pool) = gravity_parallel::installed_pool() else { return Ok(false) };
    let claims = gravity_parallel::PartitionRange::new(width as u64, 1);
    let failed = |_: Box<types_error::PgError>| gravity_driver::Error::Host(std::borrow::Cow::Borrowed("a parallel index leaf walk failed"));
    match pool.run(width, &claims, &|ord, _| job(ord).map_err(failed)) {
        Ok(()) => Ok(true),
        Err(f) => Err(crate::glue::pg_error(f.error)),
    }
}
