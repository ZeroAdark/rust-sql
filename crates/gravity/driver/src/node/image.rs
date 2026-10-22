//! The Node's image: its bytes per role and its build in the arena (split out of `node/mod.rs`, contract-runtime
//! landing 3).

use super::*;

/// The program pins group images (`min / max(text)`'s `MinVar` / `MaxVar` slots; N13: a keyed numeric extreme's side copy,
/// `NumSideMin` / `NumSideMax`): its owners take the image cut of the share.
pub fn program_images(program: &Program) -> bool {
    use gravity_abi::breaker::SlotKind;
    program.pipelines.iter().any(|p| p.owner.as_ref().is_some_and(|o| o.layout.slot_kinds().iter().any(|k| matches!(k, SlotKind::MinVar | SlotKind::MaxVar | SlotKind::NumSideMin | SlotKind::NumSideMax))))
}

pub(crate) const fn round8(n: usize) -> usize {
    n.div_ceil(8) * 8
}

#[derive(Clone, Copy)]
pub(crate) enum Role {
    Leader,
    Worker,
}

/// The widest `Pipeline.consumes` of the program: the `Partials.more` table's entries.
pub(crate) fn max_consumes(program: &Program) -> usize {
    program.pipelines.iter().map(|p| p.consumes.len()).max().unwrap_or(0)
}

pub(crate) fn image_bytes(program: &Program, derive: &dyn Derive, role: Role) -> usize {
    let l = program.layout;
    let nsites = program.sites.len();
    let out = l.out_rows as usize * l.out_natts as usize;
    let mut bytes = l.ctx_size() + round8(out * 8) + round8(out) + round8(program.out_region as usize) + round8(l.scratch_bytes as usize) + PARTIALS_SIZE + 8 * max_consumes(program) + IDENTITY_SIZE + 8 * program.instr.len();
    for d in program.sites.iter() {
        bytes += round8(d.natts() * 8) + round8(d.natts()) + round8(d.nkeys() * 8) + round8(d.nkeys()) + d.nlanes() * LANE_SIZE + d.arena_bytes();
        if d.is_source() {
            bytes += round8(derive.size_hint(d)) + 8;
        }
    }
    if matches!(role, Role::Leader) {
        if program.write.cmd.writes() {
            bytes += round8(derive.writer_size_hint()) + 8;
        }
        bytes += program.writes.len() * (round8(derive.writer_size_hint()) + 8) + round8(program.writes.len() * size_of::<(u32, ArenaBox<dyn Writer>)>());
        bytes += nlocks(program) * (round8(derive.locker_size_hint()) + 8) + round8(nlocks(program) * size_of::<LockSlot>());
    }
    bytes += round8(size_of::<Inner>()) + round8(nsites * size_of::<Option<SourceBox>>()) + round8(nsites * size_of::<SiteDesc>()) + round8(nsites * size_of::<ScanPos>()) + round8(size_of::<SpillTable>());
    bytes += round8(nrow_marks(program) * size_of::<RowMark>());
    if cfg!(debug_assertions) {
        bytes += round8(nsites * size_of::<ScanPos>()); // `Inner::positions_eager`
    }
    if matches!(role, Role::Leader) {
        let (nslots, np) = (program.slots.nslots(), program.pipelines.len());
        // `Node::build`'s leader parts: the slot regions, `live` (a bit per slot), `opened` (a bit per pipeline),
        // `producers` (an `opened`-wide bit set per slot; D3933: unreserved, it ran an 80-way join's arena out) and the
        // schedule
        let pw = gravity_program::laneset::words_for(np);
        bytes += round8(nslots * size_of::<SlotRegion>()) + gravity_program::laneset::words_for(nslots) * 8 + pw * 8 + nslots * pw * 8 + crate::lives::Schedule::<ArenaRef>::arena_bytes(program);
    }
    bytes + round8(nregions(nsites) * size_of::<(usize, usize)>()) + round8(l.exec_bytes as usize) + CONTROL_SLACK
}

/// The program's RowOf mark sites (`Inner::row_marks`).
fn nrow_marks(program: &Program) -> usize {
    program.sites.iter().filter(|d| matches!(d.kind, gravity_program::SiteKind::RowOf { .. })).count()
}

pub(crate) const fn nregions(nsites: usize) -> usize {
    10 + nsites * 6 // the `Partials.more` table is the eighth, the out block's by-reference region the ninth, EXPLAIN ANALYZE's cells the tenth
}

pub(crate) struct Image {
    pub(crate) inner: ArenaBox<Inner>,
    pub(crate) regions: ArenaVec<(usize, usize)>,
    pub(crate) image_bytes: usize,
}

pub(crate) fn owner_ptr(owner: Option<&dyn core::any::Any>) -> *mut u8 {
    owner.map_or(core::ptr::null_mut(), |o| o as *const dyn core::any::Any as *const u8 as *mut u8)
}

thread_local! {
    /// D2355: the host states (`owner_ptr`) of the programs whose pipeline code runs on this thread now, outermost first
    /// (`OwnerRun`). The list grows to the thread's deepest nesting of such calls once and keeps its capacity. D4073: beside
    /// it, the count of each owner past the list's first `SCANNED` (a deep recursion's every level asks `owner_running`).
    static RUNNING: core::cell::RefCell<(Vec<usize>, std::collections::BTreeMap<usize, u32>)> = const { core::cell::RefCell::new((Vec::new(), std::collections::BTreeMap::new())) }; // alloc: Cold
}

/// D4073: the nesting `owner_running` scans; a scan of the whole list made a deep recursion O(depth^2).
const SCANNED: usize = 16;

/// D2355: one pipeline call of a program with host state: its owner listed in `RUNNING` for the call, taken off on every
/// way out of it (its return, a suspension's, an error's, an unwind).
pub(crate) struct OwnerRun(bool);

impl OwnerRun {
    pub(crate) fn enter(owner: *mut u8) -> OwnerRun {
        if owner.is_null() {
            return OwnerRun(false);
        }
        RUNNING.with(|r| {
            let (list, deep) = &mut *r.borrow_mut();
            if list.len() >= SCANNED {
                *deep.entry(owner as usize).or_insert(0) += 1;
            }
            list.push(owner as usize);
        });
        OwnerRun(true)
    }
}

impl Drop for OwnerRun {
    fn drop(&mut self) {
        if self.0 {
            RUNNING.with(|r| {
                let (list, deep) = &mut *r.borrow_mut();
                let Some(o) = list.pop() else { return };
                if list.len() >= SCANNED {
                    if let Some(n) = deep.get_mut(&o) {
                        *n -= 1;
                        if *n == 0 {
                            deep.remove(&o);
                        }
                    }
                }
            });
        }
    }
}

/// D2355: a pipeline call on this thread's stack runs a program whose host state is `owner` (`Program::owner`'s data
/// pointer) now: a statement started inside one of its host calls (a PL/pgSQL FETCH of a cursor) would share that state
/// with it.
pub fn owner_running(owner: *const u8) -> bool {
    let o = owner as usize;
    RUNNING.with(|r| {
        let (list, deep) = &*r.borrow();
        list[..list.len().min(SCANNED)].contains(&o) || (list.len() > SCANNED && deep.contains_key(&o))
    })
}

pub(crate) fn build_image(a: ArenaRef, program: &Program, derive: &mut dyn Derive, role: Role, owner: *mut u8) -> Image {
    let l = program.layout;
    let nsites = program.sites.len();
    let out_rows = l.out_rows as usize;
    let out_natts = l.out_natts as usize;
    let ctx = a.alloc(l.ctx_size()) as *mut Ctx;
    assert_eq!(a.offset_of(ctx as *const u8), 0);
    let mut regions: ArenaVec<(usize, usize)> = ArenaVec::with_capacity_in(nregions(nsites), a);
    let take = |regions: &mut ArenaVec<(usize, usize)>, n: usize| {
        let p = a.alloc(n);
        regions.push((a.offset_of(p), n));
        p
    };
    let out_values = take(&mut regions, out_rows * out_natts * 8) as *mut Datum;
    let out_isnull = take(&mut regions, out_rows * out_natts);
    let out_region = take(&mut regions, program.out_region as usize);
    let scratch = take(&mut regions, l.scratch_bytes as usize);
    let partials = take(&mut regions, PARTIALS_SIZE) as *mut Partials;
    let consumed_more = take(&mut regions, 8 * max_consumes(program)) as *mut gravity_abi::Word<*mut u8>;
    // the breaker regions are buffers beside the arena (`SlotRegion`, `WorkerSlot::fixed_region`): a slot is zeroed when its producer opens
    let identity = take(&mut regions, IDENTITY_SIZE) as *mut Identity;
    // EXPLAIN ANALYZE's counter cells (ENTRY E1): an exec region, so zero at every bind; none for a plain program
    let instr = take(&mut regions, 8 * program.instr.len()) as *mut u64;
    let mut descs: ArenaVec<SiteDesc> = ArenaVec::with_capacity_in(nsites, a);
    raw::point_sites(ctx);
    for (i, d) in program.sites.iter().enumerate() {
        let values = take(&mut regions, d.natts() * 8) as *mut Datum;
        let isnull = take(&mut regions, d.natts());
        let keys = take(&mut regions, d.nkeys() * 8) as *mut Datum;
        let keys_null = take(&mut regions, d.nkeys());
        let lanes = take(&mut regions, d.nlanes() * LANE_SIZE) as *mut Lane;
        let s = raw::lay_site(ctx, i, (values, isnull, keys, keys_null, lanes), d.nlanes() as u16); // D1340: table_lane's bound (the array is the descriptor's)
        regions.push((a.offset_of(s as *const u8) + site::WINDOW_CELLS.0, site::WINDOW_CELLS.1));
        descs.push(d.copy_in(a));
    }
    let mut positions: ArenaVec<ScanPos> = ArenaVec::with_capacity_in(nsites, a);
    positions.resize(nsites, ScanPos::NONE);
    let image_bytes = a.used();
    let mut row_marks: ArenaVec<RowMark> = ArenaVec::with_capacity_in(nrow_marks(program), a);
    for (i, d) in program.sites.iter().enumerate() {
        if let gravity_program::SiteKind::RowOf { of } = d.kind {
            let of_positioned = program.sites.get(of as usize).is_some_and(|o| o.is_positioned());
            row_marks.push(RowMark { site: i as u32, of: of as u32, of_positioned, taken: ScanPos::NONE });
        }
    }
    #[cfg(debug_assertions)]
    let positions_eager = {
        let mut v: ArenaVec<ScanPos> = ArenaVec::with_capacity_in(nsites, a);
        v.resize(nsites, ScanPos::NONE);
        v
    };
    let mut sources: ArenaVec<Option<SourceBox>> = ArenaVec::with_capacity_in(nsites, a);
    for d in program.sites.iter() {
        sources.push(if d.is_source() {
            // D3014O: a worker's copy of a site a worker probes is the host's worker form (a frozen-image btree probe); a
            // worker's copy of an index-only site the pool reads, its share of the leaf walk; of an ordered site the pool
            // reads in key ranges, its claims' ranges
            let mut src = if matches!(role, Role::Worker) && (d.worker_probe() || d.leaves() || d.ranges()) { derive.derive_worker_source(d, a) } else { derive.derive_source(d, a) };
            if !d.zone.is_empty() {
                src.zone(d.zone_preds(), d.filter_lanes.words(), d.answer_lanes.words());
            } else if !d.filter_lanes.is_empty() {
                // V4R-340: a hit-only site (VERDICT_ELIDE_HIT, no zone predicate) hands the host the filter-only lanes a
                // condition-cache hit leaves undecoded; no predicate and no answer lane, so nothing is pruned or answered
                src.zone(&[], d.filter_lanes.words(), &[]);
            }
            if !d.zone_any.is_empty() {
                src.zone_any(&d.zone_any, &d.zone_any_ends, d.zone.len());
            }
            Some(src)
        } else {
            None
        });
    }
    let leader = matches!(role, Role::Leader);
    let writer = if leader && program.write.cmd.writes() { Some(derive.derive_writer(&program.write, a)) } else { None };
    let mut writers: ArenaVec<(u32, ArenaBox<dyn Writer>)> = ArenaVec::with_capacity_in(if leader { program.writes.len() } else { 0 }, a);
    for w in program.writes.iter().filter(|_| leader) {
        let key = if w.plan.conflict == gravity_program::Conflict::Update { w.site | crate::hostimpl::dml::OWNS_NEXT } else { w.site };
        writers.push((key, derive.derive_writer_beside(&w.plan, w.node, a)));
    }
    // D2605 / D3449: the writers that bind at their CTE's first pull, or at the execution's turn before their post group
    // (node/ends.rs)
    let lazy = super::ends::lazy_slots(program, writer.is_some(), writers.len());
    let mut lockers: ArenaVec<LockSlot> = ArenaVec::with_capacity_in(if leader { nlocks(program) } else { 0 }, a);
    if leader && program.write.lock != LockKind::None && program.locks.is_empty() {
        lockers.push(LockSlot { rel: None, host: derive.derive_locker(&program.write, a) });
    }
    for l in program.locks.iter().filter(|_| leader) {
        lockers.push(LockSlot { rel: Some(l.relid), host: derive.derive_locker(&l.plan(), a) });
    }
    let mut inner = ArenaBox::new_in(
        Inner {
            ctx,
            descs,
            positions: core::cell::UnsafeCell::new(positions),
            positions_stale: core::cell::Cell::new(false),
            row_marks,
            #[cfg(debug_assertions)]
            positions_eager,
            sources,
            writer,
            lockers,
            recheck_pos: ScanPos::NONE,
            out_natts,
            counts_pushed: program.write.cmd == Cmd::Select || program.write.cte, // a CTE's write: the SELECT's rows count
            pushed: 0,
            scratch,
            scratch_end: raw::region_end(scratch, l.scratch_bytes as usize),
            block: ProgramBlock::NONE,
            out: Out { values: gravity_abi::word(out_values), isnull: gravity_abi::word(out_isnull), count: 0, cap: out_rows as i64, region: gravity_abi::word(out_region), region_cap: program.out_region as i64, region_len: 0 },
            out_home: (out_region, program.out_region as i64),
            chunks: Vec::new(), // alloc: Node
            chunk_next: 0,
            spills: Vec::new(), // alloc: Node
            owner_maps: Vec::new(), // alloc: Node
            pin_map: (0, 0),
            bigs: Vec::new(), // alloc: Node
            work_maps: Vec::new(), // alloc: Node
            op_owner: None,
            feed_owner: None,
            own_kind: None,
            stmt_owner: None,
            spill_epoch: crate::hostimpl::arena::next_spill_epoch(),
            pinned: 0,
            stmt_pins: Vec::new(), // alloc: Node
            slot_pins: Vec::new(), // alloc: Node
            w: 0,
            mapped: 0,
            mapped_live: 0,
            mapped_peak: 0,
            w_only: false,
            stmt: core::ptr::null(),
            partials,
            consumed_more,
            identity,
            attached: None,
            err: None,
            pin_debt: None,
            spill: core::ptr::null_mut(),
            spill_index: 0,
            own_slot: 0,
            consumes: &[],
            spill_parts: false,
            refill_off: 0,
            fill_base: core::ptr::null_mut(),
            readers: Vec::new(), // alloc: Node
            home: crate::mem::grow::Home::None,
            grew: crate::mem::grow::Grew::default(),
            revoke_seen: 0,
            lift_seen: 0,
            revoke_tried: 0,
            spill_noted: (0, 0),
            writers,
            lazy,
            lazy_all: lazy,
            claim_cut: None,
        },
        a,
    );
    // D1706: every pointer the image hands out to its Inner (the program block's and each site's env, the ctx's program and
    // out) is the Box's own pointer, so the Node's later derefs of `inner` are its children and a write through them never
    // takes the hosts' and the kernels' access away (Tree Borrows); a reborrow here was a sibling of every later deref.
    let (env, al) = ArenaBox::into_raw_with_allocator(inner);
    let mut inner = raw::inner_back(env, al);
    inner.block = crate::bind::program_block(role, partials, env as *mut u8, owner);
    inner.block.instr = gravity_abi::word(if program.instr.is_empty() { core::ptr::null_mut() } else { instr });
    raw::bind_image(env, ctx, role, (scratch, inner.scratch_end), nsites as u16);
    regions.push((hdr::FLAGS, 4));
    regions.push((hdr::RESUME, 8));
    assert_eq!(regions.len(), nregions(nsites));
    Image { inner, regions, image_bytes }
}

#[cfg(test)]
mod owner_run_tests {
    use super::{owner_running, OwnerRun};

    #[test]
    fn owner_run_lists_its_owner_for_the_call_only() {
        let (a, b) = (0x10usize as *mut u8, 0x20usize as *mut u8);
        assert!(!owner_running(a));
        {
            let _outer = OwnerRun::enter(a);
            assert!(owner_running(a) && !owner_running(b));
            {
                let _unhosted = OwnerRun::enter(core::ptr::null_mut());
                let _inner = OwnerRun::enter(b);
                assert!(owner_running(a) && owner_running(b));
            }
            assert!(owner_running(a) && !owner_running(b));
        }
        assert!(!owner_running(a));
    }

    /// D4073: the calls past `SCANNED` are found through their counts; an owner nested again stays running until its outer
    /// call ends; every call comes off.
    #[test]
    fn owners_past_the_scanned_calls_are_counted() {
        let at = |i: usize| (0x100 + i * 0x10) as *mut u8;
        let n = super::SCANNED * 4;
        let mut runs = Vec::with_capacity(n);
        (0..n).for_each(|i| runs.push(OwnerRun::enter(at(i))));
        assert!((0..n).all(|i| owner_running(at(i))) && !owner_running(at(n)));
        assert_eq!(super::RUNNING.with(|r| r.borrow().1.len()), n - super::SCANNED);
        for again in [at(1), at(n - 1)] {
            let inner = OwnerRun::enter(again);
            drop(inner);
            assert!(owner_running(again), "its outer call still runs it");
        }
        while let Some(r) = runs.pop() {
            drop(r);
        }
        assert!((0..n).all(|i| !owner_running(at(i))));
        assert!(super::RUNNING.with(|r| r.borrow().1.is_empty()), "every counted call came off");
    }
}
