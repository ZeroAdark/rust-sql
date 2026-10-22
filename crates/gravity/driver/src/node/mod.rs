use std::mem::size_of;
use std::ptr::NonNull;
use std::sync::Arc;

use gravity_abi::ctx::{hdr, site_ptr, Out, Partials, FLAG_PENDING_SHIFT, LANE_SIZE, PARTIALS_SIZE};
use gravity_abi::program::ProgramBlock;
use gravity_abi::site::site;
use gravity_abi::{Ctx, Datum, Identity, Lane, MorselRange, ScanPos, IDENTITY_SIZE};
use gravity_program::{Cmd, LockKind, Program, SiteDesc, WritePlan};

use crate::mem::arena::{Arena, ArenaBox, ArenaRef, ArenaVec, Mark, SourceBox};
use crate::binding::{Attached, Binding};
use crate::mem::grant::{Counters, Share};
use crate::mem::spill::{region_len, RunReader, SpillTable};
use crate::slots::WorkerSlot;
use crate::source::{Locker, Source, Writer};
use crate::{Error, Result};

mod image;
mod locks;
mod ends;
pub use ends::{StmtEnds, STMT_ENDS};
mod pull;
mod raw;
mod slotlife;
mod ownspill;
pub use ownspill::{OWN_SPILL, SORT_RUN_SEALED};
pub use crate::mem::region::*;
pub use self::image::*;
pub use self::locks::*;
pub(crate) use self::pull::*;

/// What a kernel call settled to (the status vocabulary's non-error outcomes; errors are `Err`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ret {
    Done,
    Suspended { out_full: bool },
    Recheck { site: u8 },
    /// D824 (V4R-298): the call stopped at its probe's first row; the lazy build of its `consumed`-th slot must run first.
    BuildWait { consumed: u8 },
}

pub trait Derive {
    fn size_hint(&self, desc: &SiteDesc) -> usize;
    fn derive_source(&mut self, desc: &SiteDesc, a: ArenaRef) -> SourceBox;
    /// D3014O: a worker's source for a site a worker probes (`SiteDesc::worker_probe`); by default `derive_source`'s, which a
    /// worker's bind traps (`Source::serves_worker`).
    fn derive_worker_source(&mut self, desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        self.derive_source(desc, a)
    }
    fn writer_size_hint(&self) -> usize;
    fn derive_writer(&mut self, plan: &WritePlan, a: ArenaRef) -> ArenaBox<dyn Writer>;
    /// D1687: the writer of a write plan beside the program's `write` (a writing CTE's); by default `derive_writer`'s.
    /// D3623: `_node` its ModifyTable's plan node (`WriteAt::node`, 0: not known).
    fn derive_writer_beside(&mut self, plan: &WritePlan, _node: i32, a: ArenaRef) -> ArenaBox<dyn Writer> {
        self.derive_writer(plan, a)
    }
    fn locker_size_hint(&self) -> usize;
    fn derive_locker(&mut self, plan: &WritePlan, a: ArenaRef) -> ArenaBox<dyn Locker>;
    /// D3253: this registry's identity, which a Node built from it keeps (`Node::builder`): a source may hold closures over
    /// the admission that bound its recipes (a bitmap's recheck and pre-check read that admission's extern parameters), so
    /// a Node parked under a plan key serves only the registry it was built from (`Entry::acquire`). 0: none (every Node
    /// built from it serves any registry).
    fn identity(&self) -> u64 {
        0
    }
    /// D3438: this backend's host state for a program filed without one (`Program::hosted`), rebuilt from its template:
    /// a Node built from this registry runs over it (`None`: the program's own `Program::owner`).
    fn owner(&self) -> Option<std::rc::Rc<dyn core::any::Any>> {
        None
    }
    /// D3128: the counter cells this registry's recipes share with the template they were cloned from (an index-only
    /// walk's heap fetches, its run bound), folded, which a Node built from it keeps (`Node::cells`): its sources count
    /// into and read those cells, so a Node parked under a content key serves another registry only when that registry
    /// carries the same cells (`Entry::acquire`). 0: none.
    fn cells(&self) -> u64 {
        0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    Built,
    Bound,
    Parked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Call {
    pub out_cap: u32,
    pub resume: bool,
}

impl Call {
    pub const fn batch(out_cap: u32) -> Call {
        Call { out_cap, resume: false }
    }

    pub const fn resume(out_cap: u32) -> Call {
        Call { out_cap, resume: true }
    }
}

/// D4631: a heap list kept across a worker slot's park (emptied at the statement's end, kept at its capacity) joins
/// `WorkerSlot::list_bytes`, so a parked set's depot charge (`Parked::held`) counts it.
pub(crate) struct Inner {
    pub(crate) ctx: *mut Ctx,
    pub(crate) descs: ArenaVec<SiteDesc>,
    /// The sites' positions as the last call settled them, laid from the ctx at the first read after that settle
    /// (`Inner::positions`): a recheck reads them at a stop, so a call never walks every site.
    pub(crate) positions: core::cell::UnsafeCell<ArenaVec<ScanPos>>,
    /// A call settled since `positions` was last laid.
    pub(crate) positions_stale: core::cell::Cell<bool>,
    /// Each RowOf mark site in site order, with the row its last call took from it (`RowMark`).
    pub(crate) row_marks: ArenaVec<RowMark>,
    /// The positions as every settle laid them before reads became lazy, kept to check `positions` at each read.
    #[cfg(debug_assertions)]
    pub(crate) positions_eager: ArenaVec<ScanPos>,
    pub(crate) sources: ArenaVec<Option<SourceBox>>,
    pub(crate) writer: Option<ArenaBox<dyn Writer>>,
    pub(crate) lockers: ArenaVec<LockSlot>,
    pub(crate) recheck_pos: ScanPos,
    pub(crate) out_natts: usize,
    pub(crate) counts_pushed: bool,
    /// Rows pushed since the last settle (the push host counts; `settle_call` credits the binding).
    pub(crate) pushed: u64,
    /// The bump window at every fresh Call: the program's zeroed scratch region.
    pub(crate) scratch: *mut u8,
    pub(crate) scratch_end: *mut u8,
    /// `Ctx.program` and `Ctx.out` point here (the arena keeps `Inner` address-stable).
    pub(crate) block: ProgramBlock,
    pub(crate) out: Out,
    /// D645: the out block's own region and its capacity (the image's), put back at the statement's end (`rehome_out`)
    /// after a row wider than it regrew the block's (`SPILL_OUT_GROW`).
    pub(crate) out_home: (*mut u8, i64),
    /// The `arena_grow` pool: chunks handed out in order per execution, kept across executions.
    pub(crate) chunks: Vec<RegionBuf>,
    pub(crate) chunk_next: usize,
    /// D641: the hosted result region's spill chunks (`arena::spill_region`) and the count of their drops: parked at the
    /// statement's end (`settle_regions`, their pages MADV_FREE'd first, D2687).
    pub(crate) spills: Vec<RegionBuf>,
    /// D491: the serial owners' chunks past the grant pool (`arena::map_owners`): temp files mapped shared, given back with
    /// the pins (the statement's end, `drop_result_spills`, or a reset).
    pub(crate) owner_maps: Vec<crate::mem::spill::TempMap>,
    /// D3035: the bump window `(next, end)` of the last `owner_maps` mapping the pins past the grant pool carve
    /// (`arena::pin_mapped`); `(0, 0)` when none. Cleared with `owner_maps`.
    pub(crate) pin_map: (usize, usize),
    /// D1327: the sort merge's big buffers (`SPILL_MERGE_BIG`): `(merge region, address, bytes)`, region 0 when free; each a
    /// `spills` region, reused by the next merge (a rescanned sort draws none past its first run's), dropped with them.
    /// D3644 (b): also `SPILL_WORK_TAKE`'s blocks, keyed by their own address while in use.
    pub(crate) bigs: Vec<(usize, usize, usize)>,
    /// D4373: the bases of the `owner_maps` a sort's `SPILL_WORK_WITHIN` block mapped past W and G: unmapped at its give
    /// (`work_give`), not held free to the statement's end, so a put's copies do not pile up as dirty file pages.
    pub(crate) work_maps: Vec<usize>,
    /// LEDGER2 step 5: the ledger owner of the breaker slot the running pipeline produces into (`own_slot`'s
    /// `SlotRegion::owner`, copied at each bind beside `own_slot`); `None` when it has none. Every region, block and map
    /// this ctx owner draws for that pipeline is charged to it at the draw (`RegionBuf::own_by`, `TempMap::own_by`).
    pub(crate) op_owner: Option<memledger::Owner>,
    /// CONVERT wave 2 (R2): the ledger owner and kind of the slot the running pipeline PRODUCES into (`Pipeline::breaker`'s
    /// slot owner, its breaker's ledger kind), set beside `op_owner`; `None` when the pipeline ends in a sink or the slot
    /// has no owner. Never `own_slot_of`'s fallback to a consumed slot or slot 0: a source's bytes belong to the breaker
    /// its pipeline feeds, else to the statement (`stmt_owner`).
    pub(crate) feed_owner: Option<(memledger::Owner, memledger::Kind)>,
    /// CONVERT wave 2 (commit 7): the ledger kind of `own_slot`'s breaker (the pipeline producing it), set beside
    /// `op_owner` (`Node::own_kind_of`); `None` for a pipeline that neither produces nor consumes a slot (`own_slot_of`'s
    /// slot-0 fallback names none of its own). The spill host's own-slot answer is `op_owner` with it
    /// (`gravity_abi::program::arena_own_owner_kind`).
    pub(crate) own_kind: Option<memledger::Kind>,
    /// CONVERT wave 2 (R3): the statement's (or the portal's) ledger owner and its release, read once on the leader at the
    /// Node's bind (`Node::apply`, the read `slot_owner_begin` makes for the slot owners' parent), copied to each worker
    /// at its section's bind; `None` unbound or outside a statement (a rig thread that began none).
    pub(crate) stmt_owner: Option<(memledger::Owner, memledger::Release)>,
    pub(crate) spill_epoch: u64,
    /// Chunks `[0..pinned)` hold the execution's one-owner regions (`arena::pin`): a Call's rewind starts behind them.
    pub(crate) pinned: usize,
    /// The bases of the chunks pinned with `PIN_STATEMENT` (`arena::pin_host`): never filed as a rerun set's pins.
    pub(crate) stmt_pins: Vec<usize>,
    /// D3236 (B): the one-owner regions an owner phase pinned for its produced slot, `(slot, chunk base)` (`section::own_all`,
    /// `own_split`, `Open::own_past_grant_at`): given back when the slot frees (`Node::free_slot`), not at the statement's end.
    pub(crate) slot_pins: Vec<(u8, usize)>,
    /// W, a worker's image cut carved out of G by the leader when its section binds (`GrantPool::carve_worker`; a worker
    /// thread cannot reach G): `arena::pin` / `grow` draw it first. Always 0 on the leader, whose draws charge G itself.
    pub(crate) w: u64,
    /// The arena bytes this owner mapped since its bind (fresh chunks, S4 / D187): a worker's is folded into G at section close.
    pub(crate) mapped: u64,
    /// D3225: of `mapped`, the bytes still held (a spill region `take` gave and `give` returned before the statement's end
    /// is out again), and their high-water mark since the bind: what a worker's arena held at once, DR-12's seen draw.
    pub(crate) mapped_live: u64,
    pub(crate) mapped_peak: u64,
    /// A worker of an admitted statement: its arena draws take W, then reserve at their size (S4, D225); set at each bind.
    pub(crate) w_only: bool,
    /// The statement's reservation while an `Exec` holds the Node (leader only; a worker's draws take capacity, D187).
    pub(crate) stmt: *const crate::mem::grant::Reservation,
    /// The `Partials` table `Header.partial` points at: own / consumed regions per pipeline.
    pub(crate) partials: *mut Partials,
    /// `Partials.more`: the open pipeline's consumed regions, every one (`max_consumes` entries).
    pub(crate) consumed_more: *mut gravity_abi::Word<*mut u8>,
    pub(crate) identity: *mut Identity,
    pub(crate) attached: Option<NonNull<dyn Attached + 'static>>,
    pub(crate) err: Option<Error>,
    /// NR-3: the SPILL-DEBT row of this call's kernel pin the pool and the temp-file road both refused on a debt home
    /// (`hostimpl::arena` pin_unserved), taken at the call's settle (`settle_call`): the kind 2 or the raise the kernel
    /// answers with names it.
    pub(crate) pin_debt: Option<gravity_program::spill::NoSpill>,
    /// The Node's `SpillTable` (heap-stable) and this owner's index in it: 0 the leader, 1 + o worker o.
    pub(crate) spill: *mut SpillTable,
    pub(crate) spill_index: usize,
    /// The slot the running pipeline produces into (its spill stream), else its first consumed slot.
    pub(crate) own_slot: u8,
    /// D1324: the running pipeline's consumed slots (`STREAM_CONSUMED | k` names `consumes[k]`) and whether it claims spill
    /// partitions (S5: such a pipe reads the spilled streams of the slots before it, declared consumed or not).
    pub(crate) consumes: &'static [u8],
    pub(crate) spill_parts: bool,
    /// Where a refill lands inside the own region (`SPILL_TARGET`); 0 at every pipeline / claim start.
    pub(crate) refill_off: usize,
    pub(crate) fill_base: *mut u8, // `SPILL_RUN_BASE`: where a run fill lands; null (every pipeline / claim start) = `Partials.own`
    /// The k-way merge's open run readers (`SPILL_RUN_OPEN` handles), this owner's; dropped at reset.
    pub(crate) readers: Vec<Option<Box<dyn RunReader>>>,
    /// D254: where `region_grow` reopens the running pipeline's produced region; the grows the leader has not folded yet.
    pub(crate) home: crate::mem::grow::Home,
    pub(crate) grew: crate::mem::grow::Grew,
    /// C5 (revoke): the last revoke generation this participant serviced (`mem::grow::revoke_home`; 0 none): each ask
    /// (`mcx::revoke`) cuts its home region once.
    pub(crate) revoke_seen: u32,
    /// Ladder 1.4 (`fair_share`): the lifted ask generation this participant last serviced (its home's cut lifted once).
    pub(crate) lift_seen: u32,
    /// D4265: the revoke generation whose cut home this participant last tried to give back (`mem::grow::give_tried`),
    /// so each cut home counts once in `GIVE_TRIED`.
    pub(crate) revoke_tried: u32,
    /// C5: the statement word and the spillable bytes this participant last noted on its statement's revoke handle
    /// (`mem::grow::note_spillable`).
    pub(crate) spill_noted: (usize, u64),
    /// D1687: the writers of the program's `writes`, each by its write site (the leader's; empty for one write plan).
    pub(crate) writers: ArenaVec<(u32, ArenaBox<dyn Writer>)>,
    /// D2605: the writer slots (bit 0 `writer`, bit `i + 1` `writers[i]`) that bind at their CTE's first pull and are not
    /// bound yet, and the program's lazy slots (node/ends.rs).
    pub(crate) lazy: u64,
    pub(crate) lazy_all: u64,
    /// skewsplit (D3320): the driving site of a running pool claim that entered a cut (`claimcut::enter`, the thread's
    /// current cut), set by the section's `claim_on` for the claim's length; `None` on the leader and between claims.
    pub(crate) claim_cut: Option<u32>,
}

/// D776: a RowOf mark site: the row its host stored names source site `of`'s own scan position, and a call's settle takes
/// it (once) in place of the window's base.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowMark {
    pub(crate) site: u32,
    pub(crate) of: u32,
    /// `of` is a positioned site: every lay overwrites it from the ctx, so the mark covers it only for the call that took
    /// it. Otherwise the mark stays, as a later call that takes none leaves it.
    pub(crate) of_positioned: bool,
    /// The row the last call took from the site (`NONE` when it took none).
    pub(crate) taken: ScanPos,
}

/// One of the Node's breaker regions, one per program slot (§2 decision 1): a buffer
/// beside the arena on lazy zero pages, taken from the process-wide region depot
/// (`park::REGIONS`) at the slot's first open — a program-constant one at its bytes, a
/// share-budgeted one grown to the largest share seen — kept by the Node (a parked Node's
/// capacity, under the Park's cap) and parked in the depot when the Node drops (never memset whole).
pub(crate) struct SlotRegion {
    pub(crate) ptr: *mut u8,
    pub(crate) bytes: u32,
    pub(crate) shared: bool,
    /// The largest share this slot has opened at (the `grown_bytes` census: a Node grows once per larger share).
    high: u32,
    /// The limit this slot's life counts (`Counters::limit`): its region's bytes, plus the images for a share-budgeted one.
    counted: u64,
    pub(crate) buf: RegionBuf,
    life: Option<std::rc::Rc<crate::mem::grant::StmtCtx>>,
    /// The most this slot's region holds before its breaker spills (`grant::ceiling_of`, the leader's `leader_ceiling` at
    /// the opening width), set at its open; 0 = none (its grows keep the W-then-G roads).
    pub(crate) ceil: u64,
    /// The forced-spill test fault on its room asks (`grow::Fault`, the grant's `spill_every`), set at its open beside `ceil`.
    pub(crate) fault: core::cell::Cell<crate::mem::grow::Fault>,
    /// C10: its pipeline's breaker spills past its grant (`Pipeline::holds`), set at its open beside `ceil`: an uncapped
    /// grow past the grant is then a road the lowering did not name (`grow::uncapped`).
    pub(crate) spills: bool,
    /// NR-1: the SPILL-DEBT row its pipeline's breaker names past its grant (`Holds::debt`), set beside `spills`: an
    /// uncapped grow past the grant is then counted as that row's (`grow::uncapped`).
    pub(crate) debt: Option<gravity_program::spill::NoSpill>,
    /// D4291: the length its open asked (the share, or a started-small region's start), set beside `bytes`, which grows
    /// with an in-place growth: what an own spill lays the region back to (`Node::put_own_sorts`).
    pub(crate) opened: u32,
    /// LEDGER2 step 5: the breaker's ledger owner (an operator owner under the statement's portal, else its statement),
    /// begun by the leader at the slot's open with its share as budget and ended at its free or the statement's end: its
    /// region, its workers' regions bound for it and every block, map and run file its pipelines draw are charged to it.
    pub(crate) owner: Option<memledger::Owner>,
}

/// D3236 (B): the bytes of the one-owner pins given back at their slot's free (`Node::give_slot_pins`), process-wide.
pub static SLOT_PINS_GIVEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// D3236 (B): slot owner pins kept to the statement's end at their slot's free (a held call, or filed elsewhere).
pub static SLOT_PINS_KEPT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

impl Drop for SlotRegion {
    fn drop(&mut self) {
        crate::park::REGIONS.put(core::mem::take(&mut self.buf));
    }
}

impl Inner {
    /// A fresh map of `bytes` since the bind (`mapped`), held until `unmapped` or the statement's end.
    #[inline]
    pub(crate) fn mapped_fresh(&mut self, bytes: u64) {
        self.mapped += bytes;
        self.mapped_live += bytes;
        self.mapped_peak = self.mapped_peak.max(self.mapped_live);
        // ladder C1 try (`inline_on_draw`): the cache bytes are taken at use
        crate::mem::reclaim::ladder_mapped(bytes);
    }

    /// D3225: `bytes` of a fresh map given back before the statement's end (a spill region's `give`); a region mapped before
    /// this bind leaves the count at 0, never under.
    #[inline]
    pub(crate) fn unmapped(&mut self, bytes: u64) {
        self.mapped_live = self.mapped_live.saturating_sub(bytes);
    }

    /// The bind's map counts back to 0 (`mapped`, `mapped_live`, `mapped_peak`).
    #[inline]
    pub(crate) fn clear_mapped(&mut self) {
        (self.mapped, self.mapped_live, self.mapped_peak) = (0, 0, 0);
    }

    /// The arena chunks this ctx owner holds (the resident peak).
    pub(crate) fn chunk_bytes(&self) -> u64 {
        self.chunks.iter().map(|c| c.len() as u64).sum()
    }

    /// Every site's position `NONE`, with no call settled since (a bind, a reset).
    pub(crate) fn clear_positions(&mut self) {
        self.positions.get_mut().fill(ScanPos::NONE);
        self.positions_stale.set(false);
        for m in self.row_marks.iter_mut() {
            m.taken = ScanPos::NONE;
        }
        #[cfg(debug_assertions)]
        self.positions_eager.fill(ScanPos::NONE);
    }
}

pub struct Node {
    /// D3253: `Derive::identity` of the registry the sources were derived from.
    pub(crate) builder: u64,
    /// D3128: `Derive::cells` of that registry: the counter cells the sources count into and read.
    pub(crate) cells: u64,
    inner: ArenaBox<Inner>,
    /// DR-12: the widest worker arena the statement's sections drew (`fold_worker_arenas`), noted at its end.
    widest_arena: u64,
    exec_regions: ArenaVec<(usize, usize)>,
    /// The execution's spill files (§2 decision 3), in the arena image (address-stable): the set
    /// created at the first spill, dropped — every file removed — at `reset` (every exit path).
    /// Declared before `arena`: every `ArenaBox` of the Node must drop before the arena frees.
    pub(crate) spill: ArenaBox<SpillTable>,
    program: Arc<Program>,
    /// The host state the image's program block names (`Derive::owner`, else `Program::owner`), alive with the Node.
    owner: Option<std::rc::Rc<dyn core::any::Any>>,
    state: NodeState,
    image_bytes: usize,
    exec_mark: Mark,
    pub(crate) workers: Vec<WorkerSlot>,
    /// P2 W1: the relations its own sources keep state on between statements (`Inner::rels_read`, read at the build: a
    /// source's relation is its recipe's), for the park's release pass (`Park::release_recorded`).
    pub(crate) rels: crate::park::RelsRead,
    /// The depot key the workers were taken under (`provision`); they go back under it at release.
    pub(crate) depot_key: Option<(gravity_program::ContentKey, u64)>,
    /// D4768: the removals' generation (`slots::REMOVED_LOG`) when `depot_key`'s set came to this Node: its put drops the
    /// set when a relation removed since is one it reads.
    pub(crate) depot_seen: u64,
    pub(crate) slots: ArenaVec<SlotRegion>,
    /// C4 SlotLife: each slot's life (live, pending, kept, its stream's opener), changed only by its methods.
    slot_life: slotlife::SlotLives,
    /// Bit per pipeline: its slot opened this execution (a resumed call does not reopen).
    opened: ArenaVec<u64>,
    /// rearm E3: per slot, its producers' bits (as `opened`'s words, `opened.len()` words a slot): what `reset_in_place`
    /// unsets at each rerun, with no walk of the program's pipelines.
    producers: ArenaVec<u64>,
    /// D2881: the next `call_claim_at` is a later claim of its pipeline's run (`Entry::claim_at` sets it; the call takes it):
    /// the call carries `FLAG_LATER_CLAIM`.
    pub(crate) later_claim: bool,
    /// Sort give-back: the statement (`mcx::revoke::statement`) and revoke generation whose ask this Node's finished sorts last
    /// answered (`give_back_sorts`, between calls), apart from its home's cut (`Inner::revoke_seen`, set at a poll too).
    sorts_revoked: (usize, u32),
    /// D1339 row C (DEV): each rerun set's pins from its last run (its first member; the chunks' bases): kept pinned past the
    /// owner's `reattach` (the owner's inner reads the set's owner regions and key images there after the run), unpinned
    /// at the set's next run (`rerun_unpin`), whose reset re-lays them; `rerun_keep`: the set and hold depth whose
    /// `reattach` keeps the run's pins (`Entry::rerun_nested`).
    rerun_pins: Vec<(u16, Vec<usize>)>,
    /// D2988: a set's last filed pin list, emptied at its next run (`rerun_keep_begin`) for that run's `unfile` to fill: a
    /// rescanned set files and unfiles its pins once per rescan, without a list allocated and freed each time.
    pins_spare: Vec<usize>,
    rerun_keep: Option<(u16, usize)>,
    /// D1339 row C (DEV; review r2 F1): the running call's first pinned chunk (`hold` sets it to the held call's cursor, the
    /// nested run's pinned prefix; `reattach` puts the held call's back): a set's filed pins at or above it move at its next
    /// run, those under it trade places at its `reattach` (D2641).
    pin_floor: usize,
    /// M8 (RT-2f): each pulled member set's state, keyed by its last member (`Entry::pull_nested`): the producers already
    /// run, and the last member's call parked between pulls (its resume word, claim and live sites) or over.
    pulls: Vec<Pull>,
    /// D824: the sites of each pipeline held while an out-of-order run goes on (`hold` .. `reattach`), innermost last (a lazy
    /// build's own probe may wait for its own lazy build, D1005; M8 nests runs): its driving site (-1: none) first, then
    /// every other site whose per-open words the held call left live (a pull reader's merge arms, an index nested loop's
    /// inner mid-scan, a probe chain). Their words are the held call's, never stale ones (`stale_site_words` skips them).
    held_sites: Vec<i16>,
    /// D3586: no site's per-open words are live since the last end-of-pipeline sweep (`ended_site_words` left none that a
    /// hold or a parked pull keeps) and no kernel call ran on the context since: the next end's sweep has nothing to reset.
    /// A many-arm Append ends one pipeline per arm, a pruned arm's with no call, and each sweep walked every site: a pruned
    /// generic EXECUTE over 2000 partitions spent 3 of its 29 ms in 4 million site reads. Cleared around every call
    /// (`call_claim_at_raw`, `call_pipeline_raw`: a nested run's sweep inside the call is the call's too) and at a bind.
    swept: core::cell::Cell<bool>,
    /// M8 (RT-1): runs nested inside a leader call still on the stack (`Entry::run_nested`): while nonzero, a stopped claim
    /// releases no source pin (the outer call's row still reads its own); its error's release is the outer call's.
    nested: u32,
    /// M8 (D1036): out-of-order runs in progress (`Entry::out_of_order`), each on its own hosted lane (`nested_lane`).
    lane_depth: u16,
    /// NEW-53: the statement grant's holds on the records resident keyed ends handed on in memory (`hand_off`): (slot, the
    /// stream's life, the hold), each released when that life ends (`settle_handed`) or with the statement (`reset`).
    handed: Vec<(u8, u32, crate::mem::grant::Held)>,
    /// exactsize B4: (pipeline, its producer's counted records) for a slot-fed hash table under `pgrust.gravity_exact_size`
    /// final (`set_late_rows`, exec's late TableSize): its share at open reads them as its rows (`sized_pipe`). Per execution.
    late_rows: Vec<(u16, u64)>,
    pub(crate) counters: Counters,
    /// What `free_after`, `arms_of` and `share` ask of the program per pipeline, built once in the arena (`lives::Schedule`).
    sched: crate::lives::Schedule<ArenaRef>,
    /// LAST: fields drop in declaration order, and every `ArenaBox` / `ArenaVec` above lives in
    /// this arena (`slots` runs `SlotRegion::drop` over its elements) — the arena frees after them.
    arena: Arena,
}

const CONTROL_SLACK: usize = 512;

/// D3957: an image arena of `bytes` the allocator or the kernel refused, as the statement's error (53200 at the glue). It
/// names no SPILL-DEBT row: the arena is the program's control memory, built before any slot opens, never a breaker's region.
pub(crate) fn refused_arena(bytes: usize) -> crate::Error {
    let pool = &crate::mem::grant::POOL;
    crate::Error::OutOfMemory { bytes: bytes as u64, held: pool.held(), capacity: pool.capacity(), why: None }
}

/// D1706: the binding a call's `Node::attach_raw` attached over, put back when the call ends.
pub(crate) struct AttachBack(*mut Node, Option<NonNull<dyn Attached + 'static>>);

/// D642: the grow chunks a statement's end parks per ctx owner (`hostimpl::arena::settle_grow_chunks`); the ones past them
/// return their pages. Never a bound on growth: `chunk_region`'s bytes are.
pub const PARK_CHUNKS: usize = 64;

impl Node {
    pub fn build(program: Arc<Program>, derive: &mut dyn Derive) -> Node {
        let arena = Arena::new(image_bytes(&program, derive, Role::Leader));
        Node::build_in(arena, program, derive)
    }

    /// D3957: `build`, with a refused image arena as the statement's `OutOfMemory` (the entry's start fails, nothing built).
    pub fn try_build(program: Arc<Program>, derive: &mut dyn Derive) -> crate::Result<Node> {
        let bytes = image_bytes(&program, derive, Role::Leader);
        let arena = Arena::try_new(bytes).ok_or_else(|| refused_arena(bytes))?;
        Ok(Node::build_in(arena, program, derive))
    }

    fn build_in(arena: Arena, program: Arc<Program>, derive: &mut dyn Derive) -> Node {
        let owner = derive.owner().or_else(|| program.owner.clone());
        let a = arena.handle();
        let Image { inner, regions, image_bytes } = build_image(a, &program, derive, Role::Leader, owner_ptr(owner.as_deref()));
        let nslots = program.slots.nslots();
        let spill = ArenaBox::new_in(SpillTable::with_slots(nslots), a);
        let mut slots: ArenaVec<SlotRegion> = ArenaVec::with_capacity_in(nslots, a);
        for s in 0..nslots {
            slots.push(SlotRegion { ptr: core::ptr::null_mut(), bytes: program.slots.bytes[s], shared: program.slots.shared[s], high: 0, counted: 0, buf: RegionBuf::empty(), life: None, ceil: 0, fault: core::cell::Cell::new(crate::mem::grow::Fault::OFF), spills: false, debt: None, opened: 0, owner: None });
        }
        let slot_life = slotlife::SlotLives::new_in(nslots, a);
        let mut opened: ArenaVec<u64> = ArenaVec::with_capacity_in(gravity_program::laneset::words_for(program.pipelines.len()), a);
        opened.resize(gravity_program::laneset::words_for(program.pipelines.len()), 0);
        let pw = opened.len();
        let mut producers: ArenaVec<u64> = ArenaVec::with_capacity_in(nslots * pw, a);
        producers.resize(nslots * pw, 0);
        for (m, pipe) in program.pipelines.iter().enumerate() {
            if let Some(s) = pipe.produces().filter(|&s| (s as usize) < nslots) {
                gravity_program::laneset::set(&mut producers[s as usize * pw..(s as usize + 1) * pw], m);
            }
        }
        let sched = crate::lives::Schedule::of_in(&program, a);
        let exec_mark = a.mark();
        let rels = inner.rels_read();
        let mut node = Node { builder: derive.identity(), cells: derive.cells(), inner, widest_arena: 0, exec_regions: regions, program, owner, state: NodeState::Built, image_bytes, exec_mark, arena, workers: Vec::new(), rels, depot_key: None, depot_seen: 0, slots, slot_life, opened, producers, later_claim: false, sorts_revoked: (0, 0), rerun_pins: Vec::new(), pins_spare: Vec::new(), rerun_keep: None, pin_floor: 0, pulls: Vec::new(), held_sites: Vec::new(), swept: core::cell::Cell::new(false), nested: 0, lane_depth: 0, handed: Vec::new(), late_rows: Vec::new(), counters: Counters::default(), spill, sched }; // alloc: Node
        node.inner.spill = &mut *node.spill as *mut SpillTable;
        node.inner.spill_index = 0;
        node
    }

    /// Whether any ctx owner (the leader or a worker) spilled this execution.
    pub fn spilled(&self) -> bool {
        self.spill.spilled()
    }

    /// Bytes on disk across every spill file of this execution.
    pub fn spilled_bytes(&self) -> u64 {
        self.spill.spilled_bytes()
    }

    /// The spill partition count of this execution (0 = none).
    pub fn spill_parts(&self) -> u32 {
        self.spill.parts()
    }

    /// The generation-0 partition count of `slot`'s stream (0 = never spilled): the third phase's claims.
    pub fn spill_parts_of(&self, slot: u8) -> u32 {
        self.spill.census_of(slot).0
    }

    /// D546: `slot`'s gen-0 parts in its live life only (`SpillTable::live_parts`): what the pipelines over it read now.
    pub fn spill_parts_live(&self, slot: u8) -> u32 {
        self.spill.live_parts(slot)
    }

    /// D3140: the record bytes `slot`'s owners fold: its phase-1 table's and its stream's files' (`Entry::owner_held_of`;
    /// D3024 (b): the drain's leader rule, exec runner/plan.rs).
    pub fn owner_held(&self, slot: u8) -> u64 {
        let t1 = gravity_abi::breaker::T1::of(self.slot_region(slot)).map_or(0, |t| t.held() * t.layout.size() as u64);
        t1 + self.spill.stream(slot).files.iter().flatten().map(|f| f.spilled_bytes()).sum::<u64>()
    }

    /// The sort runs `slot`'s stream holds (every owner's), as a kernel's `SPILL_RUNS` reads them.
    pub fn sort_runs(&self, slot: u8) -> u32 {
        self.spill.stream(slot).total_runs()
    }

    /// D3190: the spill table, read only (a cut over a slot's put runs reads their fences and records:
    /// `SpillTable::run_fences` / `run_records`, while the runs are frozen past their producer).
    pub fn spill_runs(&self) -> &crate::mem::spill::SpillTable {
        &self.spill
    }

    /// The spill generations slot 0's stream reached (0 none, 1 the scatter, 2+ splits on refill).
    pub fn spill_generations(&self) -> u8 {
        self.spill.census_of(0).1
    }

    /// Drop every spill file and the set (every exit path: `reset`).
    pub(crate) fn drop_spills(&mut self) {
        // D3203: a statement that ran workers deletes its spill files across them
        self.spill.clear_at(self.workers.len());
        self.handed.clear();
    }

    /// Scatter the leader's table of pipeline `p`'s slot into the leader's file (index 0) by the
    /// breaker's `Spill::Partitioned { bits }`: the merge target overflowed, or the execution
    /// spilled and the emit's refill must find every record in the files. Returns the records
    /// moved (0 when the breaker does not spill or the table is empty).
    pub(crate) fn spill_leader(&mut self, p: usize) -> Result<u64> {
        let Some(b) = self.program.pipelines[p].breaker() else { return Ok(0) };
        let gravity_abi::breaker::Spill::Partitioned { bits } = b.spill else { return Ok(0) };
        let region = self.slot_region(b.slot);
        // a `Partitioned` breaker's region is a record or build table (`breakers.md`). A v4 build (`TABLE_V4BUILD`) writes its
        // own partitions from its lowered code and reads them back in its pair pipeline; the driver never scatters one.
        if raw::leader_unscattered(region) {
            return Ok(0);
        }
        let ptr = region.as_mut_ptr();
        // the kernel's bits are the stream's once it spilled (the seal joins the workers' epochs)
        let bits = match self.spill.stream(b.slot).bits0() {
            0 => bits,
            b => b,
        };
        let n = raw::scatter_own(&mut self.spill, b.slot, 0, ptr, bits)?;
        self.counters.spills.set(self.counters.spills.get() + 1);
        Ok(n)
    }

    /// Scatter a worker's whole partial (a live table at `ptr`) into the leader's file of pipeline
    /// `p`'s slot: the partial alone exceeds what the emptied leader table holds.
    pub(crate) fn spill_partial(&mut self, p: usize, ptr: *mut u8) -> Result<u64> {
        let Some(b) = self.program.pipelines[p].breaker() else { return Ok(0) };
        let gravity_abi::breaker::Spill::Partitioned { bits } = b.spill else { return Ok(0) };
        // a worker's live partial; a v4 build's is never scattered by the driver (`spill_leader`)
        if raw::partial_unscattered(ptr) {
            return Ok(0);
        }
        let bits = match self.spill.stream(b.slot).bits0() {
            0 => bits,
            b => b,
        };
        let n = raw::scatter_own(&mut self.spill, b.slot, 0, ptr, bits)?;
        self.counters.spills.set(self.counters.spills.get() + 1);
        Ok(n)
    }

    /// NEW-53 (D1443): pipeline `p` ends in a resident keyed end (a hashed aggregate or DISTINCT whose records keep key copies,
    /// emit_key.rs `resident`: `Spill::None`), whose arms continuing a run hand their phase-1 tables on (`hand_off`).
    pub(crate) fn hands_off(&self, p: usize) -> bool {
        use gravity_abi::breaker::{BreakerKind, Spill};
        self.program.pipelines[p].breaker().is_some_and(|b| matches!(b.spill, Spill::None) && matches!(b.kind, BreakerKind::HashAgg | BreakerKind::Distinct))
    }

    /// NEW-53 (D1443): the live phase-1 table at `ptr` (a worker's partial, or the leader's region before a section of an arm
    /// run) of resident keyed end `p` handed to the run's last arm, whose owner phase folds it with its own (the word form's
    /// `spill_partial`, never a spill of the records: they point at key copies in statement-pinned chunks, `program::pin`,
    /// D1392, which the last arm's owner fold reads before the statement ends or the rerun set runs again, D1339). In memory
    /// while the statement's grant holds the records' bytes; else (a run-time shortfall spills, never raises: the serial
    /// form answered without an error) in a disk spill file, read back by this process before the chunks go. Returns the records moved.
    pub(crate) fn hand_off(&mut self, p: usize, ptr: *mut u8) -> Result<u64> {
        let Some(b) = self.program.pipelines[p].breaker() else { return Ok(0) };
        let bytes = raw::t1_record_bytes(ptr);
        if bytes == 0 {
            return Ok(0);
        }
        self.settle_handed();
        // an admitted statement's G room (`hold` never borrows past G for it), an unadmitted one's pool capacity (a pull's)
        let held = self.inner.stmt().filter(|s| !s.is_strict() && (!s.admitted() || bytes as u64 <= s.left())).and_then(|s| s.hold(bytes as u64));
        let memory = held.is_some();
        let n = raw::hand_own(&mut self.spill, b.slot, ptr, bytes, memory)?;
        if let Some(h) = held {
            self.handed.push((b.slot, self.spill.stream(b.slot).life, h)); // alloc: Exec
        } else {
            self.counters.spills.set(self.counters.spills.get() + 1);
        }
        Ok(n)
    }

    /// NEW-53: the leader's own records of resident keyed end `p` (an arm run's earlier serial arm kept them in its table,
    /// D2648) handed on before a section of the run, as `spill_leader` scatters the word form's.
    pub(crate) fn hand_leader(&mut self, p: usize) -> Result<u64> {
        let Some(b) = self.program.pipelines[p].breaker().filter(|_| self.hands_off(p)) else { return Ok(0) };
        let region = self.slot_region(b.slot);
        if region.len() < gravity_abi::breaker::HASHAGG_HDR || raw::leader_unscattered(region) {
            return Ok(0);
        }
        let ptr = region.as_mut_ptr();
        self.hand_off(p, ptr)
    }

    /// NEW-53: the grant's holds on handed records whose stream's life ended (or began again) released.
    fn settle_handed(&mut self) {
        if !self.handed.is_empty() {
            let spill = &self.spill;
            self.handed.retain(|&(slot, life, _)| spill.stream(slot).life == life && life != 0);
        }
    }

    /// After pipeline `p` produced (serially or as a section): when its slot's stream spilled,
    /// seal the leader's remainder into the files so a refill sees every record.
    pub(crate) fn seal_spill(&mut self, p: usize) -> Result<()> {
        if let Some(b) = self.program.pipelines[p].breaker() {
            if self.spill.stream(b.slot).spilled() {
                self.spill_leader(p)?;
            }
        }
        Ok(())
    }

    /// The pipeline's own slot for the spill host: what it produces, else what it consumes first.
    pub(crate) fn own_slot_of(&self, p: usize) -> u8 {
        let pipe = self.program.pipelines[p];
        pipe.produces().or(pipe.consumes.first().copied()).unwrap_or(0)
    }

    /// D1324: pipeline `p`'s consumed slots and whether it claims spill partitions (the spill host's `stream_of`).
    pub(crate) fn reads_of(&self, p: usize) -> (&'static [u8], bool) {
        let pipe = self.program.pipelines[p];
        (pipe.consumes, matches!(pipe.claims, gravity_program::ClaimKind::SpillPartitions { .. }))
    }

    pub fn program(&self) -> &Arc<Program> {
        &self.program
    }

    /// The host state its program block names (`build`), the workers' too (`provision`).
    pub(crate) fn owner_ptr(&self) -> *mut u8 {
        gravity_abi::PtrWord::ptr(self.inner.block.owner)
    }

    /// D3438: whether its program block names `owner` (a parked Node serves only the registry whose host state it runs over).
    pub(crate) fn runs_over(&self, owner: &std::rc::Rc<dyn core::any::Any>) -> bool {
        self.owner.as_ref().is_some_and(|o| std::rc::Rc::ptr_eq(o, owner))
    }

    /// `grant::share` of pipeline `p`, the program's images flag read once (`lives::Schedule`).
    pub(crate) fn share(&self, p: usize, grant: crate::mem::grant::Grant, width: u16) -> crate::mem::grant::Split {
        match self.sized_pipe(p, grant) {
            Some(pipe) => crate::mem::grant::share_of(&self.program, p, &pipe, grant, width, self.sched.images),
            None => crate::mem::grant::share_with(&self.program, p, grant, width, self.sched.images),
        }
    }

    /// exactsize B4: pipeline `p` (a slot-fed hash table) reads `rows`, its producer's counted records, as its rows when it
    /// opens this execution (`share`, `share_left`), in place of its planner rows (0 for a serial walk of a slot).
    pub fn set_late_rows(&mut self, p: usize, rows: u64) {
        match self.late_rows.iter_mut().find(|r| r.0 as usize == p) {
            Some(r) => r.1 = rows,
            None => self.late_rows.push((p as u16, rows)), // alloc: Exec
        }
    }

    /// exactsize B4: pipeline `p` as the grant reads it under `EXACT_FINAL` when a late count was set (`set_late_rows`):
    /// its `predicted_rows` the count, which the planner's rows at its end never narrow (D4047). `None`: the program's own.
    fn sized_pipe(&self, p: usize, grant: crate::mem::grant::Grant) -> Option<gravity_program::Pipeline> {
        let rows = self.late_rows.iter().find(|r| r.0 as usize == p)?.1;
        (grant.exact & crate::mem::grant::EXACT_FINAL != 0).then(|| gravity_program::Pipeline { predicted_rows: rows, end_rows: 0, ..self.program.pipelines[p] })
    }

    pub fn state(&self) -> NodeState {
        self.state
    }

    pub fn ctx(&self) -> *mut Ctx {
        self.inner.ctx
    }

    pub fn image(&self) -> &[u8] {
        self.arena.bytes(self.image_bytes)
    }

    pub fn arena_used(&self) -> usize {
        self.arena.handle().used()
    }

    /// The Node's capacity in bytes (the arena, the slot regions, the chunk pool, its sources' kept buffers, the workers it
    /// holds): the park's cap counts it.
    pub fn bytes(&self) -> usize {
        self.arena.handle().capacity() + self.slots.iter().map(|s| s.buf.len()).sum::<usize>() + self.inner.chunks.iter().map(|c| c.len()).sum::<usize>() + self.inner.kept_bytes() + self.workers.iter().map(WorkerSlot::bytes).sum::<usize>()
    }

    pub fn exec_arena(&self) -> ArenaRef {
        self.arena.handle()
    }

    pub fn out_count(&self) -> usize {
        self.inner.out.count as usize
    }

    /// D732: each COPY mark site's position (`positions`, the stopped node's) for a recheck, flagged `POS_COPY` (its scan
    /// emits that one row); `None` clears them before the node goes back to the pool.
    pub fn set_copies(&mut self, positions: Option<&[ScanPos]>) {
        // a COPY site is positioned: the last call's positions are laid before its ctx word changes
        self.inner.lay_positions();
        for i in 0..self.inner.descs.len() {
            if matches!(self.inner.descs[i].kind, gravity_program::SiteKind::Copy) {
                let p = positions.and_then(|p| p.get(i)).filter(|p| p.is_valid()).map_or(ScanPos::NONE, |p| ScanPos { flags: p.flags | gravity_abi::ctx::POS_COPY, ..*p });
                // the node's context block holds a site slot per descriptor
                raw::site_at(self.inner.ctx, i).pos = p;
            }
        }
    }

    /// The sites' positions as the last call settled them (a source site's `pos`; `NONE` otherwise).
    pub fn positions(&self) -> &[ScanPos] {
        self.inner.positions()
    }

    pub fn nslots(&self) -> usize {
        self.slots.len()
    }

    /// Whether pipeline `pipeline` is open (its first claim, or an out-of-order run, opened it; a rerun's reset clears it).
    pub fn is_opened(&self, pipeline: usize) -> bool {
        gravity_program::laneset::has(&self.opened, pipeline)
    }

    pub(crate) fn mark_opened(&mut self, pipeline: usize) {
        gravity_program::laneset::set(&mut self.opened, pipeline);
    }

    pub(crate) fn clear_opened(&mut self) {
        self.opened.fill(0);
    }

    /// D824 (V4R-298): lazy build `b`'s slot opened zeroed at `share` before `b` runs (its outer-first probe's pipeline opens
    /// over it: every consumed slot must be live), marked pending; `b`'s own open adopts it (`open_slot`).
    pub(crate) fn open_pending(&mut self, b: usize, share: Share, images: u32) -> Result<()> {
        let Some(slot) = self.program.pipelines[b].produces() else { return Ok(()) };
        self.open_slot(slot, b, self.share_left(slot, b, share, images), images, 1)?;
        self.slot_life.pend(slot);
        Ok(())
    }

    /// M8 (RT-2e): rerun member `m` stepped past in the producer order (`Entry::skip_rerun`; PG re-executes the set on each
    /// chgParam from `owner`, the pipeline that calls `m8_rerun`): the slot it produces is kept for the statement and marked
    /// never run, and opened zeroed now when `owner` reads it (every consumed slot is live at an open, as D824's
    /// `open_pending`). `owner` calls `m8_rerun` before it reads a row, so an owner no row reaches reads an empty region; any
    /// other pipeline opening over a never-run kept slot is a Bug (`rerun_reader_check`).
    /// D1430: a slot member `m` consumes that a pipeline outside the set produced and already ran (an unparameterized
    /// HashBuild: PG keeps its hash table across the rescans) is kept for the statement too: its last consumer in program
    /// order is a member, so `free_after` freed it after the member's first run and the next rerun read a dead slot.
    /// D1431: with `every` (a rerun member, `skip_rerun`: `join::rerun_set` leases every slot it consumes), a slot it
    /// consumes that is not live yet is kept too (a pulled producer's, run later by the CTE's pull: a recursive term's
    /// once-run Hash build). A pulled set's last member (`skip_pulled`) keeps only the live ones: its others free at their
    /// last consumer. D3520: a CTE whose producers ran ahead (`CTE_PRODUCERS_AHEAD`) keeps none the program holds through
    /// its readers: each producer's slot lives through the CTE's last reader (`Pipeline::live_through`), where `free_after`
    /// frees it, and the lowering may produce that number again past it.
    pub(crate) fn defer_rerun(&mut self, m: usize, owners: &[usize], every: bool, share: Share, images: u32) -> Result<()> {
        let consumes: &'static [u8] = self.program.pipelines[m].consumes;
        let ahead = !every && self.program.v4.as_deref().is_some_and(|d| d.protos.iter().any(|x| x.kind == gravity_program::protos::ProtoKind::Cte && x.pipeline as usize == m && x.arg & gravity_program::protos::CTE_PRODUCERS_AHEAD != 0));
        for &s in consumes {
            if !every && !self.slot_life.is_live(s) {
                continue;
            }
            if ahead &&self.program.producer_of_slot(m, s).is_some_and(|q| owners.iter().all(|&o| self.program.last_consumer(q) >= o)) {
                continue;
            }
            self.slot_life.keep_consumed(s, every);
        }
        let Some(slot) = self.program.pipelines[m].produces() else { return Ok(()) };
        if owners.iter().any(|&o| o >= self.program.pipelines.len()) {
            return Err(Error::Refused("graph: a kept member's owner past the program (M8 RT-2e)".into()));
        }
        // an Append-armed member range produces one slot: its first arm opens it
        if !self.slot_life.is_kept(slot) && owners.iter().any(|&o| self.program.pipelines[o].consumes.contains(&slot)) {
            self.open_slot(slot, m, self.share_left(slot, m, share, images), images, 1)?;
        } else if !self.slot_life.is_live(slot) {
            // D2242: a member no owner reads opens only when its set runs, nested inside the owner's pipeline beside the
            // shares live there (the sort of an InitPlan a correlated SubPlan's rerun re-arms, D1262): it waits, so a share
            // opened meanwhile leaves it its part of the grant (`share_left`)
            self.defer(m);
        }
        self.slot_life.keep_unrun(slot, owners, every);
        Ok(())
    }

    /// D3515: rerun member slot `slot` kept for the statement (`defer_rerun` keeps those of the members the runner steps
    /// past), from the set's first run on: `free_after` leaves it and `reset_in_place` resets it at every run.
    pub(crate) fn keep_member(&mut self, slot: u8) {
        self.slot_life.keep(slot);
    }

    /// M8 (RT-2e): kept slot `slot` produced again by a rerun: its region, life and grant kept (no draw), its header prefix
    /// zeroed (a share re-armed), its spill stream's life restarted (the old life's files dropped); the member's open adopts
    /// it. A slot not kept or not live (a member not yet run: its first open draws as any) is left alone. Which slots: those
    /// of the members the program's lifetimes declare for the run (`Entry::rerun_nested`), and a re-armed pulled set's
    /// (`Entry::rearm_pulled`); this only says how.
    pub(crate) fn reset_in_place(&mut self, slot: u8) {
        if !self.slot_life.is_kept(slot) || !self.slot_live(slot) {
            return;
        }
        // its members open again (their sites' per-open words, their partials; `open_slot` adopts the region)
        let pw = self.opened.len();
        for (o, p) in self.opened.iter_mut().zip(&self.producers[slot as usize * pw..(slot as usize + 1) * pw]) {
            *o &= !p;
        }
        self.spill.begin_life(slot);
        self.settle_handed();
        let r = &mut self.slots[slot as usize];
        // the kept slot's own live region of `bytes`, laid as `open_slot` lays it
        raw::lay_region(r.ptr, r.bytes, r.shared.then_some(r.bytes));
        if r.shared {
            self.counters.armed(r.bytes);
        }
        self.slot_life.reset_kept(slot);
    }

    /// D1262: rerun member `p`, which produces no slot (an InitPlan's cell end), opens again at its next run (`reset_in_place`
    /// re-opens a slot's producers so).
    pub(crate) fn reopen(&mut self, p: usize) {
        gravity_program::laneset::unset(&mut self.opened, p);
    }

    /// D2701: every slot pipeline `p` consumes is kept for the statement (`free_after` leaves it, `settle_regions` frees it).
    pub(crate) fn keep_consumed(&mut self, p: usize) {
        let consumes: &'static [u8] = self.program.pipelines[p].consumes;
        for &s in consumes {
            self.slot_life.keep(s);
        }
    }

    /// D1339 row C (DEV): set `set`'s run starts (`Entry::rerun_nested`): its last run's pins unpinned (the chunks to the free
    /// tail, nothing reads them past the reset), and `reattach` at this depth files this run's; the previous keep back after.
    /// D2641: a pin under the running call's floor (a pull's, a lazy build's or an Init run's `reattach` kept it under the
    /// held call's pins: a recursive term's group table inside the CTE's pulled driver, one pull per generation) cannot move,
    /// the held calls count the chunks below their floor: it stays filed, and this run's `file_pins` trades it for a new pin.
    pub(crate) fn rerun_keep_begin(&mut self, set: u16) -> Option<(u16, usize)> {
        if let Some(i) = self.rerun_pins.iter().position(|(s, _)| *s == set) {
            let mut bases = self.rerun_pins.swap_remove(i).1;
            let stale = self.unpin(&bases);
            if !stale.is_empty() {
                self.rerun_pins.push((set, stale)); // alloc: Exec
            }
            bases.clear();
            self.pins_spare = bases;
        }
        self.rerun_keep.replace((set, self.held_sites.len()))
    }

    /// The pinned chunks of `bases` at or over the running call's floor move to the free tail; the bases under the floor
    /// are returned, still pinned (the held calls count the chunks below their floor). A base no longer pinned is dropped.
    pub(crate) fn unpin(&mut self, bases: &[usize]) -> Vec<usize> {
        let n = &mut self.inner;
        let mut stale = Vec::new(); // alloc: Exec
        for &p in bases {
            match n.chunks[..n.pinned].iter().position(|c| c.as_ptr() as usize == p) {
                Some(j) if j >= self.pin_floor => {
                    let c = n.chunks.remove(j);
                    (n.pinned, n.chunk_next) = (n.pinned - 1, n.chunk_next - 1);
                    let at = n.chunk_next;
                    n.chunks.insert(at, c); // alloc: Exec
                }
                Some(_) => stale.push(p), // alloc: Exec
                None => {}
            }
        }
        stale
    }

    pub(crate) fn rerun_keep_end(&mut self, prev: Option<(u16, usize)>) {
        self.rerun_keep = prev;
    }

    /// `reattach` after set `set`'s run: the `kept` chunks its members pinned, which `keep_nested` moved into the pinned
    /// prefix from `from` on, filed as the set's (no other set's any more), so the set's next run unpins them. D2641: the
    /// set's pins still filed here are its last run's that lay under the floor (`rerun_keep_begin`): each trades places
    /// with one of this run's and leaves the pinned prefix (`trade_stale`), so a set re-run inside a resumed pull holds one
    /// run's pins, never one per pull.
    pub(crate) fn file_pins(&mut self, set: u16, from: usize, kept: usize) {
        let mut bases = self.unfile(from, kept);
        // a statement pin (a keyed emit's kept array, `PIN_STATEMENT`) is no set's: its cache word outlives the set's runs
        bases.retain(|b| !self.inner.stmt_pins.contains(b));
        if let Some(i) = self.rerun_pins.iter().position(|(s, _)| *s == set) {
            let stale = self.rerun_pins.swap_remove(i).1;
            let n = &mut self.inner;
            let at = (n.chunk_next, n.pinned);
            let (left, at) = pull::trade_stale(&mut n.chunks, |c| c.as_ptr() as usize, &stale, from, kept, at);
            (n.chunk_next, n.pinned) = at;
            bases.extend(left); // alloc: Exec
        }
        self.rerun_pins.push((set, bases)); // alloc: Exec
    }

    /// The bases of chunks `[from, from + n)`, taken off every set's filed pins (review r2 F1: they are the filing set's
    /// now; a set's next run never moves one under its floor, D2641).
    pub(crate) fn unfile(&mut self, from: usize, n: usize) -> Vec<usize> {
        let mut bases = core::mem::take(&mut self.pins_spare);
        bases.clear();
        bases.extend(self.inner.chunks[from..from + n].iter().map(|c| c.as_ptr() as usize)); // alloc: Exec
        self.rerun_pins.iter_mut().for_each(|(_, l)| l.retain(|p| !bases.contains(p)));
        self.unfile_pulls(&bases);
        bases
    }

    /// M8 (RT-2e): a rerun's resets its members did not open (an error or a cancel stopped the run first) are dropped: those
    /// of `members`' slots only. D3374: a set run nested inside an outer set's member (a re-armed recursive CTE's term set,
    /// run by the member's pull of the CTE) leaves the outer set's resets for its later members to adopt.
    pub(crate) fn rerun_done(&mut self, members: &[u16]) {
        for &m in members {
            if let Some(slot) = self.program.pipelines.get(m as usize).and_then(|x| x.produces()) {
                self.slot_life.drop_reset(slot);
            }
        }
    }

    /// M8 (RT-2e): a member of a rerun set run again by `run_nested` without `rerun_nested`'s reset: its pipeline is still
    /// open, so its breaker would fold the second run into the first's rows (never reopened): a Bug by name.
    pub(crate) fn rerun_without_reset(&self, members: &[u16]) -> Result<()> {
        for &m in members {
            let kept = self.program.pipelines.get(m as usize).and_then(|x| x.produces()).is_some_and(|s| self.slot_life.is_kept(s));
            if kept && self.is_opened(m as usize) {
                return Err(Error::Refused(format!("gravity: Bug: rerun set member {m} run again without its reset (Entry::rerun_nested, M8 RT-2e)").into())); // alloc: Cold
            }
        }
        Ok(())
    }

    /// M8 (RT-2e): whether `slot` is a rerun set's slot no member has produced yet this execution.
    pub fn is_rerun_unrun(&self, slot: u8) -> bool {
        self.slot_life.is_unrun(slot)
    }

    /// M8 (RT-2e): a never-run kept slot is read only by its rerun owner, which reruns the set before it reads a
    /// row; any other reader would read an empty region where PG reads the set's rows: a Bug refused by name, never a value.
    /// D3914: a pass of a pipeline (`Pipeline::pass_of`: a spilled join's pair pipeline, a HAVING split's or a top-N's
    /// second pass) consumes that pipeline's slots and runs its ops again: it opens where that pipeline may. Its own reads
    /// of the set are that pipeline's, each of which runs the set first, or are none (the pair pass's rows start at the
    /// probe, past the nest or SubPlan that reads it).
    fn rerun_reader_check(&self, p: usize) -> Result<()> {
        if !self.slot_life.any_unrun() {
            return Ok(());
        }
        let pass = self.program.pipelines[p].pass_of();
        for &c in self.program.pipelines[p].consumes.iter() {
            if let Some(o) = self.slot_life.unrun_reader(c, p, pass) {
                debug_assert!(false, "pipeline {p} opens over slot {c} of a rerun set that never ran (its owner is pipeline {o})");
                return Err(Error::Refused(format!("gravity: Bug: pipeline {p} reads slot {c} of a rerun set that never ran; only its owner, pipeline {o}, opens over it (M8 RT-2e)").into())); // alloc: Cold
            }
        }
        Ok(())
    }

    /// D824: lazy build `b` has run: its slot is the probe's table now.
    pub(crate) fn built_pending(&mut self, b: usize) {
        if let Some(slot) = self.program.pipelines[b].produces() {
            self.slot_life.built(slot);
        }
    }

    /// D824: whether slot `slot` is a lazy build's that has not run.
    pub fn is_pending(&self, slot: u8) -> bool {
        self.slot_life.is_pending(slot)
    }

    /// D4358: a reader committed to slot `slot`'s run as it lies before its pipeline opened (merge.cosplit's cut laid its
    /// claims' row ranges over the resident run): the revoke's sort give-back and an own spill leave it (`own_spillable`),
    /// since a put run under those ranges is a co-split claim past a put inner's first row. Until the slot opens again.
    pub fn hold_as_laid(&mut self, slot: u8) {
        self.slot_life.hold_as_laid(slot);
    }

    /// D824: `FLAG_PENDING_SHIFT + i` for each consumed slot `i` (< 8) of pipeline `p` whose lazy build has not run.
    fn pending_flags(&self, p: usize) -> u32 {
        if !self.slot_life.any_pending() {
            return 0;
        }
        let cs = self.program.pipelines[p].consumes.iter().take(8).enumerate();
        cs.filter(|&(_, &c)| self.is_pending(c)).fold(0, |f, (i, _)| f | 1 << (FLAG_PENDING_SHIFT + i as u32))
    }

    /// The leader's `Partials`, grow home and own slot pointed at pipeline `q`'s regions again (a re-attached call: `reattach`,
    /// a parked pull resumed, `unpark_pull`).
    /// D3294: a consumed region that is no longer live is pointed at as null (`consumed_ptrs_after`) and is the error
    /// returned once the rest is pointed.
    fn point_at(&mut self, q: usize) -> Result<()> {
        let (mut partials, dead) = match self.consumed_ptrs(q) {
            Ok(p) => (p, None),
            Err(e) => (self.consumed_ptrs_after(q), Some(e)),
        };
        partials.own = match self.program.pipelines[q].breaker() {
            Some(b) => {
                self.inner.home = crate::mem::grow::Home::Slot(&mut self.slots[b.slot as usize]);
                gravity_abi::word(self.slot_ptr(b.slot))
            }
            None => {
                self.inner.home = crate::mem::grow::Home::None;
                partials.consumed[0]
            }
        };
        self.inner.own_slot = self.own_slot_of(q);
        self.inner.op_owner = self.slot_owner(self.inner.own_slot);
        self.inner.own_kind = self.own_kind_of(q);
        self.inner.feed_owner = self.feed_owner_of(q);
        (self.inner.consumes, self.inner.spill_parts) = self.reads_of(q);
        self.inner.set_partials(partials);
        dead.map_or(Ok(()), Err)
    }

    /// LEDGER2 step 5: breaker slot `slot`'s ledger owner (`SlotRegion::owner`); `None` past the slots or when it has none.
    /// Public for the runner side (exec, which holds the Node): a conversion there reaches the operator's owner by slot,
    /// e.g. `OpArena::scratch_of(node.slot_owner(slot), kind)` (CONVERT-RECIPE.md section 2.3).
    pub fn slot_owner(&self, slot: u8) -> Option<memledger::Owner> {
        self.slots.get(slot as usize).and_then(|r| r.owner)
    }

    /// CONVERT wave 2 (R2): the ledger owner of the slot pipeline `p` PRODUCES into and its breaker's ledger kind
    /// (`Inner::feed_owner`); `None` for a pipeline that ends in a sink or a slot with no owner (never `own_slot_of`'s
    /// fallback to a consumed slot or slot 0).
    pub(crate) fn feed_owner_of(&self, p: usize) -> Option<(memledger::Owner, memledger::Kind)> {
        let b = self.program.pipelines.get(p)?.breaker()?;
        Some((self.slot_owner(b.slot)?, crate::mem::region::pipeline_kind(&self.program, p)))
    }

    /// CONVERT wave 2 (commit 7): the ledger kind of pipeline `p`'s own slot (`own_slot_of`: the slot it produces into,
    /// else its first consumed one), its producing breaker's (`kind_of`); `None` for a pipeline that neither produces nor
    /// consumes a slot (no slot of its own: `own_slot_of`'s 0 is a fallback).
    pub(crate) fn own_kind_of(&self, p: usize) -> Option<memledger::Kind> {
        let pipe = self.program.pipelines.get(p)?;
        let slot = pipe.produces().or(pipe.consumes.first().copied())?;
        Some(crate::mem::region::ledger_kind(crate::mem::grow::kind_of(&self.program, slot)))
    }

    /// CONVERT wave 2 (R3): the statement's (or the portal's) ledger owner and release, captured once on the leader at this
    /// Node's bind (`apply`); `None` unbound or outside a statement. Public for the runner side (exec): the bytes a runner
    /// holds for the statement, no breaker's, are charged to it (Kind::Region).
    pub fn stmt_owner(&self) -> Option<(memledger::Owner, memledger::Release)> {
        self.inner.stmt_owner
    }

    pub(crate) fn slot_ptr(&self, slot: u8) -> *mut u8 {
        self.slots[slot as usize].ptr
    }

    pub fn slot_bytes(&self, slot: u8) -> u32 {
        self.slots[slot as usize].bytes
    }

    /// The most slot `slot`'s region holds before its breaker spills (`SlotRegion::ceil`, set at its open); 0 = none.
    pub fn slot_ceil(&self, slot: u8) -> u64 {
        self.slots[slot as usize].ceil
    }

    pub fn slot_live(&self, slot: u8) -> bool {
        self.slot_life.is_live(slot)
    }

    pub fn accounting(&self) -> crate::mem::grant::Accounting {
        self.counters.snapshot()
    }

    /// Open `slot` for its producing pipeline: its region mapped at the first open (a
    /// share-budgeted one grown to `share`, once per larger share, and armed with it); every
    /// region zeroed over its header prefix.
    pub(crate) fn stmt_ptr(&self) -> *const crate::mem::grant::Reservation {
        self.inner.stmt
    }

    pub(crate) fn set_stmt(&mut self, stmt: *const crate::mem::grant::Reservation) {
        self.inner.stmt = stmt;
    }

    /// RULE 7: the bound statement's reservation it has not drawn (G's room; 0 unbound or unadmitted).
    pub(crate) fn stmt_room(&self) -> u64 {
        self.inner.stmt().map_or(0, crate::mem::grant::Reservation::left)
    }

    /// D2242: pipeline `p` runs only on demand (`Entry::skip_deferred`): its share-budgeted slot may open nested inside a
    /// later reader's pipeline, beside that reader's live share, so the shares opened meanwhile leave it room (`share_left`).
    pub fn defer(&mut self, p: usize) {
        if let Some(b) = self.program.pipelines[p].breaker() {
            if b.slot < 64 && self.slots[b.slot as usize].shared {
                self.slot_life.defer(b.slot);
            }
        }
    }

    /// D2242: a serial open's share, cut to what the grant has left beside the share-budgeted slots already live, split
    /// with the on-demand members still to open (`defer`) (at least `SHARE_FLOOR`, page-aligned). The split sizes shares by
    /// the breakers its plan counts live at once; a member run nested inside its reader's pipeline (a lazy InitPlan's sort,
    /// a rerun set) opens beside the reader's live share uncounted: both sized to the whole grant held twice it (L2), and a
    /// reader holding the whole grant leaves the member nothing. A slot already live, or a plan-counted open with no member
    /// waiting, keeps `share`. D3088: a hash breaker never below the part `grant::share_with` charged producer `p` for its
    /// kind's PostgreSQL floor (D2587 / D2894, `grant::charged_share`): cut under it, it grows in place up to its floor
    /// anyway, doubling past the cut, and the doublings (a CTE's pulled HashAgg cut from 2.5 GiB to 1 GiB grew to its 4 GiB
    /// floor) took a grouped join written as a MATERIALIZED CTE past the statement's cap where the same plan inlined fits.
    fn share_left(&self, slot: u8, p: usize, share: Share, images: u32) -> Share {
        let Some(g) = self.inner.stmt().map(|s| s.grant) else { return share };
        let grant = g.bytes;
        let s = slot as usize;
        if !self.slots[s].shared || self.slot_life.is_live(slot) {
            return share;
        }
        let waiting = self.slot_life.deferred_besides(slot);
        // plan-counted: the slots live now, this one with them, are no more than the plan's peak for it (`share_live`)
        let others = (0..self.slots.len()).filter(|&o| o != s && self.slot_life.is_live(o as u8)).count();
        if waiting == 0 && others < self.program.slots.share_live(Some(slot)) as usize {
            return share;
        }
        let held: u64 = (0..self.slots.len()).filter(|&o| o != s && self.slots[o].shared && self.slot_life.is_live(o as u8)).map(|o| self.slots[o].counted).sum();
        let left = grant.saturating_sub(held).saturating_sub(images as u64) / (1 + waiting) & !(crate::park::REGION_MIN as u64 - 1);
        let charged = self.sized_pipe(p, g).or_else(|| self.program.pipelines.get(p).copied()).map_or(0, |pipe| crate::mem::grant::charged_share(&pipe, g));
        crate::mem::grant::cut_share(share, left, charged)
    }

    /// Slot `slot` opened for producer `p` (`open_slot_at`). A refused open of a region whose breaker names a SPILL-DEBT
    /// row (`SlotRegion::debt`, set before the open draws) names it (`grow::debt_refused`).
    pub(crate) fn open_slot(&mut self, slot: u8, p: usize, share: Share, images: u32, width: u16) -> Result<()> {
        self.open_slot_at(slot, p, share, images, width).map_err(|e| crate::mem::grow::debt_refused(e, self.slots[slot as usize].debt, None))
    }

    fn open_slot_at(&mut self, slot: u8, p: usize, share: Share, images: u32, width: u16) -> Result<()> {
        if self.slot_life.is_live(slot) {
            // D824: a lazy build adopts the slot its probe's pipeline opened over it, zeroed (`open_pending`); M8 (RT-2e): a
            // rerun member adopts its kept slot, opened zeroed for the owner (`defer_rerun`) or reset in place
            if self.slot_life.adopt(slot, self.program.pipelines[p].lazy_for >= 0) {
                return Ok(());
            }
            // D393 (V4R-47): a later arm of an append.arms run folds on into the region its first arm opened (D559: a run
            // holding producer `p`, not another run on the slot's number)
            if self.arms_of(slot, p).is_some() {
                return Ok(());
            }
            if crate::grant_ledger::on() {
                crate::grant_ledger::violation(crate::grant_ledger::Check::L3, self.inner.stmt().map_or(0, |s| s.id()), format_args!("slot {slot} produced again while its life is live"));
            }
            return Err(crate::Error::Refused(format!("graph: slot {slot} produced again while a consumer of its live life is pending (L3)").into())); // alloc: Cold
        }
        self.spill.begin_life(slot);
        self.settle_handed();
        // G1-A: the slot's region counts under its pipeline's breaker kind in the ledger's per-kind split
        let _kind = memledger::with_kind(crate::mem::region::pipeline_kind(&self.program, p));
        self.slot_life.begin_stream(slot, p);
        let r = &mut self.slots[slot as usize];
        let mut bytes = if r.shared { share.bytes } else { r.bytes };
        let shared = r.shared;
        let had = r.buf.len();
        // D285: G's room gone (an admitted statement grown past it) puts the region past G, counted as grown, with no life
        let mut past = false;
        // option c': a set operation side's region binds inside a reservation of its rows bound (`reserved_region`: one the
        // depot parked uncommitted taken back, else a fresh one), charged at the share as a past-G region is (no life)
        // D2894: so does a breaker's region whose PostgreSQL floor passes its modeled share (`grow::floor_reserve`)
        let grant = self.inner.stmt().map(|s| s.grant);
        // the breaker's ceiling: the leader's at the opening width (`leader_ceiling`), the region's reservation and its grows' bound
        r.ceil = match grant {
            Some(g) if r.shared => {
                let pipe = &self.program.pipelines[p];
                pipe.breaker().map_or(0, |b| crate::mem::grant::leader_ceiling(crate::mem::grant::ceiling_of(g, pipe), width, b.kind))
            }
            _ => 0,
        };
        r.fault.set(crate::mem::grow::Fault::every(grant.map_or(0, |g| g.spill_every)).with_revoke_every(grant.map_or(0, |g| g.revoke_every)).with_refuse_debt(grant.is_some_and(|g| g.refuse_debt)));
        let holds = if self.program.lowered() { self.program.pipelines[p].holds() } else { None };
        (r.spills, r.debt) = (holds.is_some_and(|h| h.spills()), holds.and_then(|h| h.debt()));
        let (mut reserve, mut floor) = if r.shared { crate::mem::grow::share_reserve(&self.program.pipelines[p], grant, bytes as u64, width, true) } else { (0, false) };
        // a hash breaker starts small: its share is the layout it grows into in place, its region commits the start
        let start = crate::mem::grant::start_bytes(bytes);
        let laid = r.shared && crate::mem::grant::starts_small_in(&self.program, p, grant.unwrap_or(crate::mem::grant::Grant::of(0))) && start < bytes;
        let top = bytes;
        if laid {
            (reserve, floor) = (crate::mem::grow::start_reserve(&self.program.pipelines[p], grant, bytes as u64, width, true), true);
            bytes = start;
        }
        if reserve > bytes as u64 || r.buf.is_reserved() {
            let r = &mut self.slots[slot as usize];
            crate::park::REGIONS.put(core::mem::take(&mut r.buf));
            let stmt = self.inner.stmt();
            r.buf = if reserve > bytes as u64 {
                past = true;
                if laid {
                    // A parked laid region's resident length is the start, past the share when it grew past it (D3794): the
                    // table's limit is the region's writable end, so its commits in place go on from there
                    let buf = crate::node::laid_region(crate::park::region_bytes(bytes as usize), reserve as usize, top as usize, None, stmt)?;
                    bytes = bytes.max(buf.len().min(buf.lay_reserve()) as u32 & !7);
                    buf
                } else {
                    let (buf, held) = crate::mem::region::reserved_share(bytes, reserve as usize, None, stmt, floor)?;
                    bytes = held;
                    buf
                }
            } else {
                match crate::park::REGIONS.take(bytes as usize, stmt) {
                    Ok(buf) => buf,
                    Err(e) => {
                        past = true;
                        past_g_open(&mut bytes, shared, stmt, e)?
                    }
                }
            };
        } else if r.buf.len() < bytes as usize {
            // a dead slot's region first (D157: a freed slot kept its region and the next breaker drew one more share)
            let dead = (0..self.slots.len()).filter(|&s| s != slot as usize && !self.slot_life.is_live(s as u8)).find(|&s| self.slots[s].buf.len() >= bytes as usize);
            if let Some(d) = dead {
                let buf = core::mem::take(&mut self.slots[d].buf);
                self.slots[d].ptr = core::ptr::null_mut();
                let old = core::mem::replace(&mut self.slots[slot as usize].buf, buf);
                self.slots[d].buf = old;
            } else {
                // the old region goes first: its bytes come back to the pool before the larger one asks
                let r = &mut self.slots[slot as usize];
                crate::park::REGIONS.put(core::mem::take(&mut r.buf));
                let stmt = self.inner.stmt();
                r.buf = match crate::park::REGIONS.take(bytes as usize, stmt) {
                    Ok(buf) => buf,
                    Err(e) => {
                        past = true;
                        past_g_open(&mut bytes, shared, stmt, e)?
                    }
                };
            }
        }
        let stmt = self.inner.stmt();
        let r = &mut self.slots[slot as usize];
        r.life = if past { None } else { stmt.and_then(|s| s.life("GravityLife", r.buf.capacity() as u64)) };
        if !past && !r.buf.recharge(stmt, r.life.as_ref()) {
            // a kept region past the grant's room: dropped for one of the bytes asked (past G when G has no room for it)
            r.buf = crate::node::RegionBuf::empty();
            r.life = None;
            match crate::park::REGIONS.take(bytes as usize, stmt) {
                Ok(buf) => {
                    r.life = stmt.and_then(|s| s.life("GravityLife", buf.capacity() as u64));
                    r.buf = buf;
                    if !r.buf.recharge(stmt, r.life.as_ref()) {
                        r.buf = crate::node::RegionBuf::empty();
                        r.life = None;
                        let e = crate::Error::OutOfMemory { bytes: bytes as u64, held: crate::mem::grant::POOL.held(), capacity: crate::mem::grant::POOL.capacity(), why: None };
                        r.buf = past_g_open(&mut bytes, shared, stmt, e)?;
                    }
                }
                Err(e) => r.buf = past_g_open(&mut bytes, shared, stmt, e)?,
            }
        }
        if region_debug() & TRACE != 0 {
            eprintln!("gravity: region: slot {slot} open shared {} share {} bytes {bytes} buf {had} -> {} drawn {}", r.shared, share.bytes, r.buf.len(), self.inner.stmt().map_or(0, |s| s.drawn()));
        }
        // a laid region taken warm past its share counts its share, as its commits in place past the share are not counted
        // here: the bytes past it are held past G (`laid_region`), the grown part of the region
        let shared_bytes = if laid { bytes.min(top) } else { bytes };
        let counted = crate::park::region_bytes(shared_bytes as usize) as u64 + if r.shared { images as u64 } else { 0 };
        self.counters.limit(counted, core::mem::replace(&mut r.counted, counted));
        if r.shared {
            self.live_shares_check(slot);
        }
        let r = &mut self.slots[slot as usize];
        if r.shared && shared_bytes > r.high {
            r.high = shared_bytes;
            self.counters.grown_bytes.set(self.counters.grown_bytes.get() + shared_bytes as u64);
        }
        r.ptr = r.buf.as_mut_ptr();
        r.bytes = bytes;
        r.opened = if laid { start.min(bytes) } else { bytes };
        // `ptr` is the slot's own region of `bytes` (arena or buffer), 8-aligned. Zeroed over its header prefix only
        // (`ZERO_PREFIX`): every table zeroes the extent it lays out itself — a memset of the whole share per statement was
        // the §2 group-by regression, a 2 MB program-constant region per worker per section track A's frame-kernel finding.
        raw::lay_region(r.ptr, r.bytes, r.shared.then_some(bytes));
        if r.shared {
            self.counters.armed(shared_bytes);
        }
        // LEDGER2 step 5: the breaker's owner (its share the budget), its region charged to it here whichever road drew,
        // kept or swapped it in, and its stream's run files posted to it
        if let Some(o) = self.slot_owner_begin(slot, p, top) {
            self.slots[slot as usize].buf.own_by(o);
            self.spill.set_owner(slot, Some(o));
        }
        self.slot_life.open(slot);
        self.counters.opens.set(self.counters.opens.get() + 1);
        Ok(())
    }

    /// LEDGER2 step 5: slot `slot`'s ledger owner: the live one with its budget set to `budget` (a reopen at another
    /// share), else one begun now (pipeline `p`'s breaker kind, `budget` bytes promised) under the owner the leader's
    /// charges take now (the portal its backend executes, else its statement). `None` when the operator table is full or
    /// that owner ended (`op_begin` answers the parent): the slot's charges stay where they were.
    fn slot_owner_begin(&mut self, slot: u8, p: usize, budget: u32) -> Option<memledger::Owner> {
        if let Some(o) = self.slots[slot as usize].owner {
            memledger::charge::op_budget(o, budget as u64);
            return Some(o);
        }
        let (parent, _) = memledger::charge::context();
        let o = memledger::charge::op_begin(parent, crate::mem::region::pipeline_kind(&self.program, p) as u32, budget as u64);
        let owner = (o != parent).then_some(o);
        self.slots[slot as usize].owner = owner;
        owner
    }

    /// LEDGER2 step 5: slot `slot`'s owner ended (its region already parked or dropped): its stream's posted run bytes
    /// taken back first, then `op_end` (the cell frees with its last byte: a spill region or map it drew that lives on to
    /// the statement's end keeps it keyed until then).
    fn slot_owner_end(&mut self, slot: u8) {
        if let Some(o) = self.slots[slot as usize].owner.take() {
            self.spill.set_owner(slot, None);
            memledger::charge::op_end(o);
        }
    }

    // L8 (I7, D182): a fresh open finds the per-open words of the sites it reads at their initial value (a site-less pipeline: every site)
    #[cold]
    /// L2 (S7c): the live share-budgeted slots' limits within `grant_ledger::l2_bound`; a debug assert, a report line under
    /// the ledger.
    fn live_shares_check(&self, slot: u8) {
        let Some((g, reserved)) = self.inner.stmt().map(|s| (s.grant, s.reserved)) else { return };
        let grant = g.bytes;
        let live = (0..self.slots.len()).filter(|&s| self.slots[s].shared && self.slot_life.is_live(s as u8) || s == slot as usize);
        let (n, sum) = live.fold((0u64, 0u64), |(n, t), s| (n + 1, t + self.slots[s].counted));
        let bound = crate::grant_ledger::l2_bound(g, n, reserved);
        if sum > bound {
            if crate::grant_ledger::on() {
                crate::grant_ledger::violation(crate::grant_ledger::Check::L2, self.inner.stmt().map_or(0, |s| s.id()), format_args!("{n} live shares hold {sum} B past the grant's {grant} B and its reservation's {reserved} B (slot {slot} opened)"));
            }
            debug_assert!(sum <= bound, "L2: {n} live shares hold {sum} B past the grant's {grant} B");
        }
    }

    /// L8 (S7c): an OPEN site (it skips the seek) or a `cont` (it resumes a probe chain) left by an earlier open: an error in debug
    /// builds, reset in release. A closed / EOF site re-seeks at its first `next`, its cursor / rows / sel_n with it.
    fn stale_site_words(&self, p: usize) -> Result<()> {
        self.sweep_site_words(p, false)
    }

    /// D2224: pipeline `p` ended (`Ret::Done`): the per-open words its run left on a site no held call and no parked pull
    /// owns are dead, and they are reset here, not found stale by the next open's L8 sweep. A run that stops early (a LIMIT
    /// or a top satisfied, an EXISTS tail's inner scan, a fused probe) leaves its scans OPEN; PostgreSQL's ExecEndNode
    /// closes them at the same point.
    pub(crate) fn ended_site_words(&self, p: usize) {
        let _ = self.sweep_site_words(p, true);
    }

    fn sweep_site_words(&self, p: usize, ended: bool) -> Result<()> {
        if ended && self.swept.get() {
            return Ok(());
        }
        let nsites = self.program.sites.len();
        let d = self.program.pipelines[p].driving_site;
        // a site with no live word is left as it is, kept or not, so only a live one asks whether a held call or a parked
        // pull keeps it: the end-of-pipeline sweep walks every site, and a many-arm Append ends one pipeline per arm
        let sites = (if d >= 0 && !ended { d as usize..(d as usize + 1).min(nsites) } else { 0..nsites }).filter(|&i| self.site_words_live(i));
        // D3586: an end's sweep that leaves no live site behind (none kept by a hold or a parked pull)
        let mut kept = false;
        for i in sites.filter(|&i| {
            let keep = self.held_sites.iter().any(|&h| h as isize == i as isize) || self.parked_site(i);
            kept |= keep;
            !keep
        }) {
            // `i` is one of the program's sites, laid out by `build` in the Node's ctx
            let s = raw::site_at(self.ctx(), i);
            let words = [("state", (s.state == gravity_abi::site::site::STATE_OPEN) as u64 * s.state as u64), ("cont", s.cont)];
            if let Some((w, v)) = words.iter().find(|(_, v)| *v != 0) {
                if ended {
                    (s.state, s.cont) = (gravity_abi::site::site::STATE_CLOSED, 0);
                    continue;
                }
                if crate::grant_ledger::on() {
                    crate::grant_ledger::violation(crate::grant_ledger::Check::L8, self.inner.stmt().map_or(0, |s| s.id()), format_args!("pipeline {p} opens with site {i}'s per-open word {w} = {v} left by an earlier open"));
                }
                if cfg!(debug_assertions) {
                    return Err(crate::Error::Host(format!("gravity: pipeline {p} opens with site {i}'s per-open word {w} = {v} left by an earlier open (L8)").into())); // alloc: Cold
                }
                (s.state, s.cont) = (gravity_abi::site::site::STATE_CLOSED, 0);
            }
        }
        if ended && !kept {
            self.swept.set(true);
        }
        Ok(())
    }

    /// The regions pipeline `p` consumes, as the `Partials` face carries them: the first two inline (null past
    /// its `consumes`), every one in the `consumed_more` table (`Partials.more`); every one must be live.
    /// D3294 (D3262's site): one that is not live (a re-attached call over a slot released under it) is the statement's
    /// error by name, never a panic (a plain panic aborts the server, D2933).
    pub(crate) fn consumed_ptrs(&self, p: usize) -> Result<Partials> {
        let mut out = Partials::NONE;
        for (k, &c) in self.program.pipelines[p].consumes.iter().enumerate() {
            if !self.slot_live(c) {
                return Err(Error::Host(format!("gravity: Bug: pipeline {p} consumes slot {c} that is not live").into())); // alloc: Cold
            }
            let ptr = self.slot_ptr(c);
            if k < 2 {
                out.consumed[k] = gravity_abi::word(ptr);
            }
            self.inner.set_consumed_more(k, ptr);
        }
        out.more = gravity_abi::word(self.inner.consumed_more);
        Ok(out)
    }

    /// D3140: `consumed_ptrs` after pipeline `p`'s arm ran: a slot its run released since (a drain arm's build) is null.
    /// The owners `Section::open_owners` binds fold the produced slot's partials and files only; they read no consumed region.
    pub(crate) fn consumed_ptrs_after(&self, p: usize) -> Partials {
        let mut out = Partials::NONE;
        for (k, &c) in self.program.pipelines[p].consumes.iter().enumerate() {
            let ptr = if self.slot_live(c) { self.slot_ptr(c) } else { core::ptr::null_mut() };
            if k < 2 {
                out.consumed[k] = gravity_abi::word(ptr);
            }
            self.inner.set_consumed_more(k, ptr);
        }
        out.more = gravity_abi::word(self.inner.consumed_more);
        out
    }

    /// D3230 (op 14, fed): only the slot pipeline `p` produces, opened as `open_pipeline` opens it, for a pipeline that never
    /// runs: a deferring build whose feeders passed unrun (`Entry::defer_fed`), its region the home of the FAILED stamp
    /// alone. No call is bound, so neither its consumed regions (a feeder after the failed one never opened its slot) nor
    /// its sites' per-open words (the failed feeder's driving site is still open mid-claim) are read.
    pub(crate) fn open_produced(&mut self, p: usize, share: Share, images: u32) -> Result<()> {
        let Some(b) = self.program.pipelines[p].breaker() else { return Ok(()) };
        self.open_slot(b.slot, p, self.share_left(b.slot, p, share, images), images, 1)
    }

    /// C5: the leader's home region cut once per revoke its statement was asked (`mem::grow::revoke_home`), between calls.
    pub(crate) fn revoke_home(&mut self) -> bool {
        crate::mem::grow::revoke_leader(&mut self.inner)
    }

    /// Before pipeline `p`'s first call on the leader: open the slot it
    /// produces (its region sized by `share` when share-budgeted) and point the
    /// leader's `Partials` at its own and consumed regions (law 4). D4291: `open_pipeline` (`ownspill`) runs it, once more
    /// after its statement's own finished sorts are put when the first try was refused on the own-spill road; nothing it
    /// sets outlives a refused try (`open_slot` makes the slot live only once its region is drawn).
    fn open_pipeline_once(&mut self, p: usize, share: Share, images: u32) -> Result<()> {
        self.stale_site_words(p)?;
        self.rerun_reader_check(p)?;
        let mut partials = self.consumed_ptrs(p)?;
        partials.own = match self.program.pipelines[p].breaker() {
            Some(b) => {
                self.open_slot(b.slot, p, self.share_left(b.slot, p, share, images), images, 1)?;
                self.inner.home = crate::mem::grow::Home::Slot(&mut self.slots[b.slot as usize]);
                gravity_abi::word(self.slot_ptr(b.slot))
            }
            None => {
                self.inner.home = crate::mem::grow::Home::None;
                partials.consumed[0]
            }
        };
        self.inner.own_slot = self.own_slot_of(p);
        self.inner.op_owner = self.slot_owner(self.inner.own_slot);
        self.inner.own_kind = self.own_kind_of(p);
        self.inner.feed_owner = self.feed_owner_of(p);
        (self.inner.consumes, self.inner.spill_parts) = self.reads_of(p);
        self.inner.refill_off = 0;
        self.inner.fill_base = core::ptr::null_mut();
        self.inner.set_partials(partials);
        Ok(())
    }

    /// The statement's end: every region the Node holds (slots, arena chunks, the workers') to the free list, the lives dropped.
    /// D3203: `t` laps the parts into `entry::END_US` (0 the slots, 1 the leader's chunks and spills, 2 the workers').
    pub(crate) fn settle_regions(&mut self, t: &mut Option<u64>) {
        // DR-12: the leader's draws past its pins (its grow chunks) and the widest worker arena, for the next run's footprint
        let grown = self.inner.chunks.iter().skip(self.inner.pinned).map(|c| c.len() as u64).sum::<u64>();
        crate::mem::grant::note_arena(self.program().key, grown, self.widest_arena);
        self.widest_arena = 0;
        for r in self.slots.iter_mut() {
            r.ptr = core::ptr::null_mut();
            crate::park::REGIONS.put(core::mem::take(&mut r.buf));
            r.life = None;
            r.counted = 0;
        }
        crate::entry::end_lap(t, 0);
        self.counters.settle_limits();
        crate::hostimpl::arena::settle_grow_chunks(&mut self.inner);
        crate::hostimpl::arena::drop_result_spills(&mut self.inner);
        self.inner.rehome_out();
        self.inner.chunk_next = 0;
        self.inner.pinned = 0;
        self.inner.stmt_pins.clear();
        self.inner.slot_pins.clear();
        self.rerun_pins.clear();
        self.pin_floor = 0;
        crate::entry::end_lap(t, 1);
        self.workers.iter_mut().for_each(WorkerSlot::settle_regions);
        // LEDGER2 step 5: every slot owner the statement left ends with its regions settled
        for slot in 0..self.slots.len() {
            self.slot_owner_end(slot as u8);
        }
        self.inner.op_owner = None;
        self.inner.feed_owner = None;
        // CONVERT wave 2 (commit 7): own_kind with op_owner, so a reader of the field alone never sees a stale kind
        self.inner.own_kind = None;
        self.workers.iter_mut().for_each(|w| (w.inner.op_owner, w.inner.own_kind, w.inner.feed_owner, w.inner.stmt_owner) = (None, None, None, None));
        crate::entry::end_lap(t, 2);
    }

    /// Section close (leader): each worker's arena maps since its bind counted in the statement's draws (S4, D187).
    pub(crate) fn fold_worker_arenas(&mut self, width: u16) {
        let (mut sum, mut max, mut w) = (0u64, 0u64, 0u64);
        for s in self.workers.iter().take(width as usize) {
            sum += s.inner.mapped;
            max = max.max(s.inner.mapped);
            // D3225: what the worker held at once, not every map since its bind (a spill region given back and taken
            // again counted each time: 2 GiB a worker over 16 GiB of draws, reserved x width at the next run whatever its grant)
            self.widest_arena = self.widest_arena.max(s.inner.mapped_peak);
            w += s.inner.w;
        }
        if let Some(s) = self.inner.stmt() {
            s.note_drawn(sum);
        }
        self.note_resident();
        if region_debug() & ARENA != 0 {
            eprintln!("gravity: arena: section width {width} workers mapped max {} KiB sum {} KiB, W left {} KiB", max >> 10, sum >> 10, w >> 10);
        }
    }

    /// The resident peak at a section close or a pipeline's end: what the Node holds now (the leader's live slot regions and
    /// arena chunks, every worker's share, fixed and arena buffers) as the statement's resident bytes; the max is the "peak".
    fn note_resident(&self) {
        let Some(s) = self.inner.stmt() else { return };
        let slots = (0..self.slots.len()).filter(|&k| self.slot_life.is_live(k as u8)).map(|k| self.slots[k].buf.len() as u64);
        s.note_resident(slots.sum::<u64>() + self.inner.chunk_bytes() + self.workers.iter().map(WorkerSlot::resident).sum::<u64>());
    }

    /// The bytes the pool charges for breaker slot `slot`'s regions now (leader thread, where `note_resident` reads): the
    /// leader's live region and each worker's partial region bound for it (`RegionBuf::charged_bytes`); 0 when the slot is
    /// not open.
    pub fn slot_charged(&self, slot: u8) -> u64 {
        let k = slot as usize;
        if k >= self.slots.len() || !self.slot_life.is_live(k as u8) {
            return 0;
        }
        self.slots[k].buf.charged_bytes() + self.workers.iter().map(|w| w.partial_charged(slot)).sum::<u64>()
    }

    /// Step 3a: breaker slot `slot`'s charged bytes now, the leader's region and the widest worker's partial, for its
    /// statement's history (`mem::feedback`), when the statement runs under feedback.
    fn observe_feedback(&self, slot: u8) {
        let key = self.program.key.0;
        if !crate::mem::feedback::live(key) {
            return;
        }
        let k = slot as usize;
        let leader = if k < self.slots.len() && self.slot_life.is_live(k as u8) { self.slots[k].buf.charged_bytes() } else { 0 };
        let worker = self.workers.iter().map(|w| w.partial_charged(slot)).max().unwrap_or(0);
        crate::mem::feedback::observe(key, slot, leader, worker, self.inner.stmt().map_or(0, |s| s.refused()));
    }

    /// Step 3a: the statement's end (`Entry::end`, before its regions settle): its samples written. A failed statement's
    /// open slot is observed as it stands.
    pub(crate) fn finish_feedback(&self, ok: bool) {
        let key = self.program.key.0;
        if !crate::mem::feedback::live(key) {
            return;
        }
        if !ok {
            self.observe_feedback(self.inner.own_slot);
        }
        let Some(grant) = self.inner.stmt().map(|s| s.grant) else { return };
        let spill = |slot: u8| self.spill.slot_spilled_bytes(slot);
        let given = |slot: u8, width: u16| self.slot_given(slot, grant, width);
        crate::mem::feedback::finish(key, ok, &spill, &given);
    }

    /// Step 3a: what the sizing gave breaker slot `slot` at `grant` over `width` ([leader, worker] bytes; the worker's 0 for
    /// a serial producer) and the room each had before a draw past it ([leader, worker]: its share, or its kind's floor the
    /// region commits up to in place when that is more).
    fn slot_given(&self, slot: u8, grant: crate::mem::grant::Grant, width: u16) -> ([u64; 2], [u64; 2]) {
        let Some(p) = self.program.pipelines.iter().position(|q| q.breaker().is_some_and(|b| b.slot == slot)) else { return ([0; 2], [0; 2]) };
        let pipe = &self.program.pipelines[p];
        let split = self.share(p, grant, width);
        let floor = crate::mem::grant::floor_for(grant, pipe);
        let leader = crate::park::region_bytes(split.breaker.bytes as usize) as u64;
        let worker = if width >= 2 && pipe.parallel == gravity_program::Parallel::Eligible { split.worker.bytes as u64 } else { 0 };
        ([leader, worker], [leader.max(floor), if worker == 0 { 0 } else { worker.max(floor) }])
    }

    pub(crate) fn arena_census(&self) {
        if region_debug() & ARENA != 0 {
            self.arena_census_line();
        }
    }

    #[cold]
    fn arena_census_line(&self) {
        let bytes = |i: &Inner| i.chunks.iter().map(|c| c.len() as u64).sum::<u64>();
        let w: Vec<u64> = self.workers.iter().map(|w| bytes(&w.inner)).collect(); // alloc: Cold
        let s = self.inner.stmt();
        eprintln!("gravity: arena: leader {} KiB workers {} max {} KiB sum {} KiB w-left {} KiB; grant {} MiB reserved {} KiB left {} KiB drawn {} KiB admitted {}", bytes(&self.inner) >> 10, w.iter().filter(|&&b| b != 0).count(), w.iter().max().copied().unwrap_or(0) >> 10, w.iter().sum::<u64>() >> 10, self.workers.iter().map(|w| w.inner.w).sum::<u64>() >> 10, s.map_or(0, |s| s.grant.bytes >> 20), s.map_or(0, |s| s.reserved >> 10), s.map_or(0, |s| s.left() >> 10), s.map_or(0, |s| s.drawn() >> 10), s.is_some_and(|s| s.g().is_some()));
    }

    /// A parked Node's regions given back under pool pressure (`Park::put`): the slot regions to
    /// the depot (which drops them over budget), the workers' share and fixed buffers dropped. The
    /// next open re-takes them (`open_slot`, `share_region`, `fixed_region` grow on a shorter buffer).
    pub fn shed_regions(&mut self) -> usize {
        let mut freed = self.shed_warm();
        for r in self.slots.iter_mut() {
            freed += r.buf.len();
            r.ptr = core::ptr::null_mut();
            crate::park::REGIONS.put(core::mem::take(&mut r.buf));
        }
        // the arena's pool chunks (the tables, up to a grant) go too: `reset` has rewound the
        // cursors, `grow`/`pin` re-take on the next statement
        freed += self.inner.chunks.iter().map(|c| c.len()).sum::<usize>();
        self.inner.chunks.clear();
        for w in self.workers.iter_mut() {
            freed += w.shed_regions();
        }
        if freed != 0 {
            crate::mem::grant::POOL.note_shed();
        }
        freed
    }

    /// After pipeline `p` completed: free every slot whose last consumer it was; end the stream of every slot whose last reader
    /// it was (S5: a spill-partition pipe reads the spilled streams of the slots before it, declared consumed or not).
    pub(crate) fn free_after(&mut self, p: usize) {
        self.note_resident();
        if let Some(slot) = self.program.pipelines[p].produces() {
            self.observe_feedback(slot);
        }
        self.free_pulled_after(p);
        // the producers due after `p` (`lives::FreeSchedule`: each producer's last consumer, an arm's at its run's last arm's,
        // D393; its stream's end at the later of that and the last spill-partition pipe before the slot's next producer)
        let due = self.sched.frees.after(p).len();
        for i in 0..due {
            let (q, flags) = self.sched.frees.after(p)[i];
            let Some(slot) = self.program.pipelines[q as usize].produces() else { continue };
            // M8 (RT-2e): a rerun set's kept slot stays for the statement, its region (`settle_regions` frees it) and its
            // stream (D1638: a member's next run re-reads a kept Sort's stream; `drop_spills` ends it with the statement)
            let kept = self.slot_life.is_kept(slot);
            if !kept && flags & crate::lives::DUE_END != 0 && self.spill.stream(slot).life != 0 && self.holds_life(q as usize, slot) {
                self.spill.end_life(slot);
            }
            if flags & crate::lives::DUE_FREE != 0 && !kept {
                self.free_slot(slot);
            }
        }
        self.settle_handed();
    }

    /// D3546: slot `slot`'s stream life is producer `q`'s: `q`'s open began it, or an earlier arm's of the append.arms run
    /// holding `q` (its later arms fold on into it, D393). `lives::FreeSchedule` ends a producer's stream after the last
    /// spill-partition pipe before the slot's next producer (S5), in program order; an InitPlan's members run on demand
    /// nested inside its reader's pipeline, so that pipe can end there after the next producer opened the slot. The life
    /// then is the next producer's (its `begin_life` dropped `q`'s), and ending it lost an ordered aggregate's Sort stream
    /// before its reader ('slot 0's stream read outside its life').
    fn holds_life(&self, q: usize, slot: u8) -> bool {
        let o = self.slot_life.opener(slot) as usize;
        o == q || self.sched.arms.run_of(q).is_some_and(|r| r.contains(&o))
    }

    /// D2498: reader `p` of a pulled set's store is done; a store with no reader left whose producer ran to its end frees now
    /// (it was kept only for them), its kept marks dropped so a later producer of the slot number opens it as any. A producer
    /// still parked or never pulled leaves its store to `settle_regions`: the statement-end drain lands its rest there (D2179).
    fn free_pulled_after(&mut self, p: usize) {
        loop {
            let program = &self.program;
            let held = |slot| program.producer_of_slot(p, slot).map(|q| program.last_consumer(q)).filter(|&end| end > p);
            let Some(slot) = self.slot_life.reader_done(p, held) else { return };
            if self.pull_done_into(slot) {
                self.slot_life.unkeep(slot);
                self.free_slot(slot);
            }
        }
    }

    /// A live slot's region goes back to the park, its life and counted bytes ended (a slot not live is left alone).
    fn free_slot(&mut self, slot: u8) {
        if !self.slot_live(slot) {
            return;
        }
        self.slot_life.free(slot);
        let stmt = self.inner.stmt();
        let r = &mut self.slots[slot as usize];
        r.ptr = core::ptr::null_mut();
        crate::park::REGIONS.put_from_life(core::mem::take(&mut r.buf), stmt);
        self.counters.limit(0, core::mem::take(&mut r.counted));
        r.life = None;
        self.counters.frees.set(self.counters.frees.get() + 1);
        self.give_slot_pins(slot);
        self.slot_owner_end(slot);
    }

    /// D4078 (ADMITFP (A)): the pin floor under which an owner phase may give back the region its
    /// round 1 failed in before its split pins a chunk (`section::own_all`'s `give`), on `give_slot_pins`' own conditions:
    /// no held call (`held_sites`), no rerun set's run (`rerun_keep`: its members' pins are filed at its end) and no pull open
    /// (`pulls`: a pulled set files the pins it made); `None` otherwise, and the region stays to the slot's free as before.
    pub(crate) fn owner_give_floor(&self) -> Option<usize> {
        (self.held_sites.is_empty() && self.rerun_keep.is_none() && self.pulls.is_empty()).then_some(self.pin_floor)
    }

    /// D3236 (B): `slot`'s one-owner regions (`Inner::slot_pins`, the owner phase's pins) given back as the slot frees: a
    /// HashAgg's merged group table lived to the statement's end after its last consumer read it (a UNION ALL of keyed
    /// aggregates at W8: each arm's 2-4 GiB owner held through the arms after it). Each leaves the pinned prefix as `unpin` moves a chunk and goes
    /// to the depot, its charge back to G or the pool. Only with no held call (`held_sites` empty: a nested run's
    /// `reattach` counts the chunks it pinned) and only a pin no rerun set, pull or statement pin files (each of those
    /// unpins or trades it by its base later); such a pin stays to the statement's end, as every owner pin did before.
    fn give_slot_pins(&mut self, slot: u8) {
        if !self.inner.slot_pins.iter().any(|&(s, _)| s == slot) {
            return;
        }
        let (free, floor) = (self.held_sites.is_empty(), self.pin_floor);
        let mut i = 0;
        while i < self.inner.slot_pins.len() {
            let (s, base) = self.inner.slot_pins[i];
            if s != slot {
                i += 1;
                continue;
            }
            self.inner.slot_pins.swap_remove(i);
            if !free || self.inner.stmt_pins.contains(&base) || self.rerun_pins.iter().any(|(_, l)| l.contains(&base)) || self.pull_pinned(base) {
                SLOT_PINS_KEPT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                continue;
            }
            let n = &mut self.inner;
            let Some(j) = n.chunks[..n.pinned].iter().position(|c| c.as_ptr() as usize == base) else { continue };
            if j < floor {
                SLOT_PINS_KEPT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                continue;
            }
            let c = n.chunks.remove(j);
            (n.pinned, n.chunk_next) = (n.pinned - 1, n.chunk_next - 1);
            SLOT_PINS_GIVEN.fetch_add(c.len() as u64, core::sync::atomic::Ordering::Relaxed);
            if region_debug() & TRACE != 0 {
                eprintln!("gravity: region: slot {slot} owner pin {} given back at its free", c.len());
            }
            crate::park::REGIONS.put_from_life(c, self.inner.stmt());
        }
    }

    /// D393 (V4R-47): the append.arms run (`first..end`, `arg` pipelines) producing `slot` (its `Arms` proto names the slot)
    /// that holds pipeline `q`: one production across the arms, the slot opened by the first arm to run and folded on by the
    /// rest. The run's first and last pipelines produce the slot; a pipeline between that produces another slot (MergeAppend's
    /// per-arm feeder, M3-MERGE) is no arm and runs untouched. Consecutive pipelines, so the slot is live between two arms
    /// only for the run. D559: a slot number is reused once its life ends, so one statement can hold two runs on one number:
    /// the run is the one whose range holds `q`, never the first proto naming the slot.
    fn arms_of(&self, slot: u8, q: usize) -> Option<core::ops::Range<usize>> {
        if self.program.pipelines.get(q).and_then(|x| x.produces()) == Some(slot) {
            return self.sched.arms.run_of(q);
        }
        let v4 = self.program.v4.as_deref()?;
        let produces = |q: usize| self.program.pipelines.get(q).and_then(|x| x.produces()) == Some(slot);
        v4.protos.iter().filter(|p| p.kind == gravity_program::protos::ProtoKind::Arms && p.slot == slot).map(|p| p.pipeline as usize..p.pipeline as usize + p.arg as usize).find(|run| run.contains(&q) && produces(run.start) && produces(run.end - 1))
    }

    /// D393b (V4R-57): pipeline `p` is an arm of an append.arms run other than its last: it folds only. The owner fold, the
    /// merge's finish and the seal wait for the run's last arm; a section's partials go to the slot's files (close_section),
    /// which the last arm's owner fold / refill reads with its own (D2648: a serial arm's records stay in the leader's table).
    /// D3918 / D4072: a run that ends at its first arm (`arm_ends_run`) is not continued: that arm owns, finishes and seals it.
    pub fn arm_continues(&self, p: usize) -> bool {
        self.program.pipelines[p].produces().and_then(|s| self.arms_of(s, p)).is_some_and(|r| p + 1 < r.end) && !self.arm_ends_run(p)
    }

    /// D3918: arm `a` produces no row: a spilled build's pair pass (its claims `ClaimKind::SpillPartitions`) over a build
    /// that wrote no partition, so no claim of it is cut (runner plan.rs `no_partition`). A lazy build that has not run
    /// (D824) may still spill: its pair pass is not known empty.
    pub fn arm_empty(&self, a: usize) -> bool {
        matches!(self.program.pipelines.get(a).map(|x| x.claims), Some(gravity_program::ClaimKind::SpillPartitions { slot }) if !self.slot_life.is_pending(slot) && self.spill_parts_live(slot) == 0)
    }

    /// D3918: a sort's, (D4072) a hash aggregate's or (D4054) a hash build's append.arms run ends at its first arm `p`
    /// when every later arm is an empty pair pass (`arm_empty`: B4b-1's `P'` under a build that never spilled). `p` is
    /// then the slot's single producer: its owner phase at its section's width, its finish and its seal, as a probe
    /// before it had a pair pass. The later arms are stepped past unopened (`arm_ended`). Continued, `p`'s workers' sorted
    /// runs or keyed partials went to the slot's files and the empty pass's serial finish folded them on the leader alone
    /// (a build's: its table was finished there, serially, at every execution). A union's run of arms (MergeAppend, a
    /// build's union arms or seeded drain) never ends early: its later arms are no pair passes.
    pub fn arm_ends_run(&self, p: usize) -> bool {
        use gravity_abi::breaker::BreakerKind::{HashAgg, HashBuild, Sort};
        let pipe = &self.program.pipelines[p];
        let Some(slot) = pipe.produces().filter(|_| pipe.breaker().is_some_and(|b| matches!(b.kind, Sort | HashAgg | HashBuild))) else { return false };
        let produces = |a: usize| self.program.pipelines[a].produces() == Some(slot);
        self.arms_of(slot, p).is_some_and(|r| r.start == p && p + 1 < r.end && (p + 1..r.end).filter(|&a| produces(a)).all(|a| self.arm_empty(a)))
    }

    /// D3918: pipeline `p` is a later arm of a run that ended at its first arm (`arm_ends_run`): it is stepped past unopened
    /// (`Entry::skip_ended`), its claims none and the run sealed.
    pub fn arm_ended(&self, p: usize) -> bool {
        self.program.pipelines[p].produces().and_then(|s| self.arms_of(s, p)).is_some_and(|r| r.start < p && self.arm_ends_run(r.start))
    }

    /// D393b: `FLAG_ARM_RUN` for a claim of pipeline `p` when it is an arm of an append.arms run (its phase-1 tables open at
    /// one partition count), else 0.
    pub(crate) fn arm_flag(&self, p: usize) -> u32 {
        if self.in_arm_run(p) { gravity_abi::ctx::FLAG_ARM_RUN } else { 0 }
    }

    /// D393b: pipeline `p` is an arm of an append.arms run (any arm, the last too).
    pub fn in_arm_run(&self, p: usize) -> bool {
        self.program.pipelines[p].produces().and_then(|s| self.arms_of(s, p)).is_some()
    }

    /// D3140: the pipelines of the append.arms run `p` is an arm of.
    pub fn arm_run(&self, p: usize) -> Option<core::ops::Range<usize>> {
        self.program.pipelines[p].produces().and_then(|s| self.arms_of(s, p))
    }

    /// D393b: the partition count the owner fold of arm `p` reads its slot's stream at: the spilled stream's own (an
    /// append.arms run's earlier arms scattered their records into files at it), 0 when `p` is no arm of a run or the
    /// stream holds no file. D4054: 0 for a hash build too. A v4 build writes its own partition files into the stream and
    /// reads them back in its pair pass: they are no record table's, its merge has no owner phase, and an owner fold over
    /// them found no owner at any size. Its run's last arm finishes as a lone build does.
    pub(crate) fn run_stream_bits(&self, p: usize) -> u8 {
        let pipe = &self.program.pipelines[p];
        match pipe.produces() {
            Some(s) if !pipe.breaker().is_some_and(|b| b.kind == gravity_abi::breaker::BreakerKind::HashBuild) && self.in_arm_run(p) => self.spill.stream(s).bits0(),
            _ => 0,
        }
    }

    /// D4054: arm `p` finished the production pipeline `of` belongs to: `of` itself when it is no arm of an append.arms
    /// run, else the run's one arm of the slot that neither continues (`arm_continues`) nor was stepped past
    /// (`arm_ended`): its last arm, or its first when the run ended there (`arm_ends_run`). A protocol that reads the
    /// finished slot (a build's empty reading, its key export) reads it after this arm, whichever pipeline declared it:
    /// after an arm that continues the slot holds only the rows so far.
    pub fn finishes(&self, of: usize, p: usize) -> bool {
        match self.arm_run(of) {
            None => p == of,
            Some(run) => run.contains(&p) && self.program.pipelines[p].produces() == self.program.pipelines[of].produces() && !self.arm_continues(p) && !self.arm_ended(p),
        }
    }

    /// D2648: an arm that continues its run (D393b) seals like any producer, only when the slot's stream spilled (its table
    /// outgrew its budget): unspilled, its records stay in the leader's table, which the next arm folds on into (`open_slot`:
    /// the region its first arm opened) and the run's last arm owns. A section's partials still go to the files
    /// (close_section: a record table has no serial fold), which spills the stream, so the leader's remainder follows them.
    pub(crate) fn end_production(&mut self, p: usize) -> Result<()> {
        self.seal_spill(p)
    }

    /// What the parked Node keeps open for the sources (addition i of §1.10).
    pub fn residency(&self) -> crate::source::Residency {
        let mut r = crate::source::Residency::default();
        for s in self.inner.sources.iter().flatten() {
            let x = s.residency();
            r.parts += x.parts;
            r.bytes += x.bytes;
        }
        r
    }

    pub fn source_mut(&mut self, i: usize) -> Option<&mut (dyn Source + 'static)> {
        self.inner.sources.get_mut(i).and_then(|s| s.as_mut()).map(|s| &mut **s)
    }

    /// D3670: pipeline `p` runs now (a producer's run, or a pulled set's generation): its driving site's source counts the
    /// scan PostgreSQL begins or rescans at that run when its bind left the count to the runs (`Source::count_scan`: a
    /// rerun set's or an InitPlan's member, whose scan PG counts once per run, and never when the member never runs).
    /// On the leader, which bound the source.
    pub fn count_scan(&mut self, p: usize) {
        let Some(site) = self.program.pipelines.get(p).and_then(|x| usize::try_from(x.driving_site).ok()) else { return };
        if let Some(s) = self.source_mut(site) {
            s.count_scan();
        }
    }

    pub fn worker_capacity(&self) -> usize {
        self.workers.len()
    }

    pub(crate) fn slot_and_workers(&mut self, slot: Option<u8>) -> (&mut [u8], &[WorkerSlot]) {
        let region = match slot {
            Some(s) => self.slot_region(s),
            None => &mut [],
        };
        (region, &self.workers)
    }

    pub(crate) fn for_each_host(&mut self, mut f: impl FnMut(HostRef<'_>)) {
        self.inner.for_each_host(&mut f);
    }

    pub fn apply(&mut self, b: &mut Binding<'_>) -> Result<()> {
        assert_eq!(self.state, NodeState::Built, "apply on a {:?} node", self.state);
        let bref = b.as_ref();
        let mut failed = None;
        self.for_each_host(|h| {
            if failed.is_none() {
                failed = h.bind(&bref).err();
            }
        });
        if let Some(e) = failed {
            // D4602: every host unbinds, the one whose bind failed and those never bound included (a worker slot's failed
            // open does the same: section.rs `bind_hosts`, then `detach`). A bind may take statement state before the step
            // that fails (D4599: a bitmap source's recheck qual), and the node parks after this error with what it holds.
            self.for_each_host(|h| h.unbind());
            return Err(e);
        }
        self.inner.clear_positions();
        // KEYED-22: the sites' rows count this execution's (a parked node carried the last one's): observe's rows folded
        for i in 0..self.program.sites.len() {
            let s = raw::site_at(self.ctx(), i);
            (s.rows_in, s.rows_out) = (0, 0);
        }
        self.swept.set(false);
        b.rows_processed = 0;
        self.inner.bind_words(b);
        self.inner.attach(b);
        // CONVERT wave 2 (R3): the statement's (or the portal's) owner, read once here on the leader (the read
        // `slot_owner_begin` makes for its slot owners' parent): the sources' and the runners' statement-owned bytes and the
        // arena owner of a pipeline that produces no slot are charged to it
        self.inner.stmt_owner = stmt_context();
        self.state = NodeState::Bound;
        Ok(())
    }

    pub fn remove(&mut self) {
        assert_eq!(self.state, NodeState::Bound, "remove on a {:?} node", self.state);
        self.cool_workers();
        self.for_each_host(|h| h.unbind());
        ends::reset(&mut self.inner);
        self.inner.unbind_words();
        self.inner.detach();
        self.inner.stmt_owner = None;
        self.state = NodeState::Built;
    }

    /// D3196 (rearm E1): the hosts a nested run's sections left bound in the worker slots (`WorkerSlot::warm`) unbound: the
    /// statement's end (`Entry::release`, before the slots' regions settle; `remove`).
    pub(crate) fn cool_workers(&mut self) {
        for w in self.workers.iter_mut() {
            w.cool();
        }
    }

    pub fn reset(&mut self) {
        self.reset_timed(&mut None);
    }

    /// `reset`, its parts lapped into `entry::END_US` (4 up to the spill table, 5 the spill table, 6 the rest) while `t` runs.
    pub(crate) fn reset_timed(&mut self, t: &mut Option<u64>) {
        assert_eq!(self.state, NodeState::Built, "reset on a {:?} node", self.state);
        self.slot_life.clear();
        self.opened.fill(0);
        self.pulls.clear();
        self.held_sites.clear();
        self.late_rows.clear();
        self.nested = 0;
        // M8 (D1036): an out-of-order run a panic unwound left its nested lane in the leader's ordinal (0)
        if core::mem::take(&mut self.lane_depth) != 0 && !self.inner.ctx.is_null() {
            self.inner.lead_ordinal();
        }
        self.inner.clear_positions();
        self.inner.readers.clear();
        for w in self.workers.iter_mut() {
            w.inner.readers.clear();
        }
        crate::entry::end_lap(t, 4);
        self.drop_spills();
        crate::entry::end_lap(t, 5);
        let a = self.arena.handle();
        for &(off, n) in self.exec_regions.iter() {
            a.zero(off, n);
        }
        a.reset_to(self.exec_mark);
        self.inner.owner_maps.clear();
        self.inner.work_maps.clear();
        self.inner.pin_map = (0, 0);
        self.inner.pinned = 0;
        self.inner.chunk_next = 0;
        self.inner.stmt_pins.clear();
        self.inner.slot_pins.clear();
        self.rerun_pins.clear();
        self.pin_floor = 0;
        self.inner.out.count = 0;
        self.inner.out.region_len = 0;
        for w in self.workers.iter_mut().map(|w| &mut w.inner.w) {
            let left = core::mem::take(w);
            if left != 0 {
                crate::mem::grant::POOL.release(left as usize);
            }
        }
        self.inner.out.cap = self.program.layout.out_rows as i64;
        self.inner.rewind_ctx();
        crate::entry::end_lap(t, 6);
    }

    pub fn park(&mut self) {
        assert_eq!(self.state, NodeState::Built, "park on a {:?} node", self.state);
        // D4630 / D4631: under the keep-warm switch its sources keep their bind-sized buffers for the next run (on the
        // session from their unbind), so its next bind sizes nothing again; the park's idle release and budget give them
        // back (`shed_regions`). Writers and lockers park as before.
        let warm = mempolicy::keep::keep_warm();
        let park = |h: HostRef<'_>| {
            if !(warm && matches!(h, HostRef::Source(_))) {
                h.park();
            }
        };
        self.for_each_host(park);
        for w in self.workers.iter_mut() {
            w.inner.for_each_host(&mut |h| park(h));
        }
        self.state = NodeState::Parked;
    }

    /// D4630 / D4631: a parked Node's warm sources parked now (the keep-warm switch left their bind-sized buffers at its
    /// park): their buffers given back. The bytes freed.
    fn shed_warm(&mut self) -> usize {
        if !mempolicy::keep::keep_warm() {
            return 0;
        }
        let before = self.inner.park_bytes() + self.workers.iter().map(|w| w.inner.park_bytes()).sum::<usize>();
        self.for_each_host(|h| {
            if let HostRef::Source(s) = h {
                s.park();
            }
        });
        for w in self.workers.iter_mut() {
            w.inner.for_each_host(&mut |h| {
                if let HostRef::Source(s) = h {
                    s.park();
                }
            });
        }
        before.saturating_sub(self.inner.park_bytes() + self.workers.iter().map(|w| w.inner.park_bytes()).sum::<usize>())
    }

    pub fn unpark(&mut self) {
        assert_eq!(self.state, NodeState::Parked, "unpark on a {:?} node", self.state);
        self.for_each_host(|h| h.unpark());
        for w in self.workers.iter_mut() {
            w.inner.for_each_host(&mut |h| h.unpark());
        }
        self.state = NodeState::Built;
    }

    pub fn teardown(self) {
        drop(self);
    }

    pub fn call(&mut self, call: Call) -> Result<Ret> {
        self.call_pipeline(0, call)
    }

    /// One claim of `pipeline` on the leader (the width-1 arm driven claim by claim, decision 13):
    /// `hdr.claim` set, a fresh run, settled like a worker's claim.
    pub fn call_claim(&mut self, pipeline: usize, claim: MorselRange) -> Result<Ret> {
        self.call_claim_at(pipeline, claim, false)
    }

    /// D254: the leader's `region_grow`s of this call counted, the statement's first logged.
    fn fold_grows(&mut self, pipeline: usize) {
        if self.inner.grew.n != 0 {
            crate::mem::grow::fold(&mut self.inner.grew, &self.counters, self.program.pipelines[pipeline].breaker().map(|b| b.kind));
        }
    }
}

/// CONVERT wave 2 (R3): the owner a statement-owned charge made now on the calling (leader) thread takes: the portal its
/// backend executes, else its running statement (`memledger::charge::context`), with its release; `None` outside both
/// (a session's owner, a thread with no backend account). `Node::apply`'s read, and a runner-level sink's built before
/// `start_at` (gravity_driver::stmt_context).
pub fn stmt_context() -> Option<(memledger::Owner, memledger::Release)> {
    match memledger::charge::context() {
        (o @ (memledger::Owner::Statement { .. } | memledger::Owner::Portal { .. }), r) => Some((o, r)),
        _ => None,
    }
}

pub(crate) enum HostRef<'a> {
    Source(&'a mut dyn Source),
    Writer(&'a mut dyn Writer),
    Locker(&'a mut dyn Locker),
}

impl HostRef<'_> {
    pub(crate) fn bind(self, b: &crate::binding::BindingRef<'_>) -> Result<()> {
        match self {
            HostRef::Source(s) => s.bind(b),
            HostRef::Writer(w) => w.bind(b),
            HostRef::Locker(l) => l.bind(b),
        }
    }

    pub(crate) fn unbind(self) {
        match self {
            HostRef::Source(s) => s.unbind(),
            HostRef::Writer(w) => w.unbind(),
            HostRef::Locker(l) => l.unbind(),
        }
    }

    pub(crate) fn release(self) {
        if let HostRef::Source(s) = self {
            s.release()
        }
    }

    /// D3196 (rearm E1): a warm worker slot's source re-armed for its next section (`Source::rearm`); a worker slot holds no
    /// writer or locker.
    pub(crate) fn rearm(self) -> Result<()> {
        match self {
            HostRef::Source(s) => s.rearm(),
            HostRef::Writer(_) | HostRef::Locker(_) => Ok(()),
        }
    }

    fn park(self) {
        match self {
            HostRef::Source(s) => s.park(),
            HostRef::Writer(w) => w.park(),
            HostRef::Locker(l) => l.park(),
        }
    }

    fn unpark(self) {
        match self {
            HostRef::Source(s) => s.unpark(),
            HostRef::Writer(w) => w.unpark(),
            HostRef::Locker(l) => l.unpark(),
        }
    }
}

impl Inner {
    /// D645: the out block back on its own region at the statement's end; a regrown one (`SPILL_OUT_GROW`) went with the
    /// result spills just dropped (`arena::drop_result_spills`).
    pub(crate) fn rehome_out(&mut self) {
        (self.out.region, self.out.region_cap) = (gravity_abi::word(self.out_home.0), self.out_home.1);
        self.out.region_len = 0;
    }

    /// D3213: the heap its sources keep between statements (`Source::kept_bytes`).
    pub(crate) fn kept_bytes(&self) -> usize {
        self.sources.iter().flatten().map(|s| s.kept_bytes()).sum()
    }

    /// D4554: what its sources' parks would give back now (`Source::park_bytes`).
    pub(crate) fn park_bytes(&self) -> usize {
        self.sources.iter().flatten().map(|s| s.park_bytes()).sum()
    }

    /// D4725: `for_each_host` without the sources of the sites in `skip` (bit i: site i).
    pub(crate) fn for_each_host_but(&mut self, skip: u64, f: &mut dyn FnMut(HostRef<'_>)) {
        if skip == 0 {
            return self.for_each_host(f);
        }
        for (i, s) in self.sources.iter_mut().enumerate() {
            if let Some(s) = s.as_mut().filter(|_| i >= 64 || skip >> i & 1 == 0) {
                f(HostRef::Source(&mut **s));
            }
        }
        debug_assert!(self.writer.is_none() && self.writers.is_empty() && self.lockers.is_empty(), "D4725: a worker slot holds sources alone");
    }

    /// P2 W1: the relations its sources read (`Source::rel_read`, nonzero), which `Source::release_rel` alone releases from.
    pub(crate) fn rels_read(&self) -> crate::park::RelsRead {
        let mut rels = crate::park::RelsRead::NONE;
        self.sources.iter().flatten().for_each(|s| rels.add(s.rel_read()));
        rels
    }

    /// D4768: whether a source of this slot reads a relation `removed` names (`Source::rel_read`).
    pub(crate) fn reads_any(&self, removed: &dyn Fn(u32) -> bool) -> bool {
        self.sources.iter().flatten().any(|s| {
            let rel = s.rel_read();
            rel != 0 && removed(rel)
        })
    }

    pub(crate) fn for_each_host(&mut self, f: &mut dyn FnMut(HostRef<'_>)) {
        for s in self.sources.iter_mut().flatten() {
            f(HostRef::Source(&mut **s));
        }
        // D2605: a writer that binds at its CTE's first pull takes part once bound (node/ends.rs)
        let lazy = self.lazy;
        if let Some(w) = self.writer.as_mut().filter(|_| lazy & 1 == 0) {
            f(HostRef::Writer(&mut **w));
        }
        // D1687: a writing CTE's writer binds after the statement's (its BEFORE / AFTER STATEMENT triggers after the
        // statement's: PG's top ModifyTable runs first, an unread writing CTE at ExecPostprocessPlan); D2772 POST / D3449:
        // in a program with post pipelines, not before the execution's turn binds it (node/ends.rs)
        for (i, (_, w)) in self.writers.iter_mut().enumerate() {
            if i >= 63 || lazy & (1 << (i + 1)) == 0 {
                f(HostRef::Writer(&mut **w));
            }
        }
        for l in self.lockers.iter_mut() {
            f(HostRef::Locker(&mut *l.host));
        }
    }

    pub(crate) fn detach(&mut self) {
        (self.block.push, self.block.sink) = (gravity_abi::word(crate::bind::UNBOUND_SINK.0), gravity_abi::word(crate::bind::UNBOUND_SINK.1));
        self.attached = None;
        self.err = None;
    }

    pub(crate) fn pins(&self) -> u64 {
        self.sources.iter().flatten().map(|s| s.pins()).sum()
    }

}

/// D3226 (c1): `past_g` for a slot's open: a shared (share-budgeted) slot's refusal opens at the pool's room
/// (`past_g_share`) and `*bytes` becomes the share it holds; any other slot's refusal stands.
fn past_g_open(bytes: &mut u32, shared: bool, stmt: Option<&crate::mem::grant::Reservation>, e: crate::Error) -> crate::Result<RegionBuf> {
    if !shared {
        return past_g(*bytes, stmt, e);
    }
    let (buf, held) = crate::mem::region::past_g_share(*bytes, stmt, e)?;
    *bytes = held;
    Ok(buf)
}
