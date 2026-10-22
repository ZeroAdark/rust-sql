use core::hash::{Hash, Hasher};

use gravity_abi::ctx::{ctx_size, lane};
use gravity_abi::hash::Fnv128;

use crate::arena::{Arena, ArenaRef, ArenaVec};
use crate::laneset::LaneSet;
use gravity_abi::breaker::{fits_inline, Breaker, Budget, KeyKind, OwnerFns, RecordLayout, SlotKind, WideLayout, LAYOUT_IMPLICIT_COUNT, MAX_RECORD_SLOTS, ORDER_KEY, ORDER_NONE};
use crate::code::Code;
use crate::lives::LifeId;
use crate::rep::{DictRef, LaneLife, NumTypmod, Rep};

mod lanes;
mod layout;
mod owner;
mod pipeline;
mod serial;
mod site;
mod write;
mod zone;
pub use self::lanes::*;
pub use self::layout::*;
pub use self::owner::*;
pub use self::pipeline::*;
pub use self::serial::*;
pub use self::site::*;
pub use self::write::*;
pub use self::zone::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ContentKey(pub u128);

pub use gravity_abi::breaker::{ACC_PAIRS, ACC_SLOT_BASE};

pub struct Program {
    /// C2 (G1-A's program caches): the bytes `file_on_ledger` moved onto the program caches' memory account.
    ledger: Ledgered,
    pub key: ContentKey,
    /// `key` without the estimates an admission binds per statement (a pipeline's `predicted_rows` / `build_rows` that
    /// name their graph pipeline, `rows_of` / `build_of`; KEY-PREDICTED-ROWS road (b)) and without a pipeline's `end_rows`
    /// (D3205; D4047: bound where `end_of` names them, else dropped by a statement of other estimates): two programs of one
    /// `bind_key` run alike once each statement's estimates are bound over them (exec admit `estimates`).
    pub bind_key: ContentKey,
    pub layout: Layout,
    pub sites: ArenaVec<SiteDesc>,
    pub out: ArenaVec<OutCol>,
    pub write: WritePlan,
    pub pipelines: ArenaVec<Pipeline>,
    pub slots: SlotPlan,
    /// The out block's by-reference region in bytes (lower.md §15.13); 0 = no by-reference output column.
    pub out_region: u32,
    codes: ArenaVec<Code>,
    arena: Arena,
    /// A lowered function's host state (the lowerer's `hosted.rs`), dropped with the program; leader-side only.
    /// Shared, not owned: `assemble` hands every program of one `Lowered` the same state (the rig re-assembles under another backend).
    pub owner: Option<std::rc::Rc<dyn core::any::Any>>,
    /// D3438: the template each backend rebuilds this program's host state from (lowerv4 `hir::rebuild`), set where the
    /// program is filed without its `owner` (the process-wide tier's): immutable, shared by every backend that runs it.
    pub hosted: Option<std::sync::Arc<dyn core::any::Any + Send + Sync>>,
    pub instr: ArenaVec<(i32, u8)>, // EXPLAIN ANALYZE's cells (ENTRY E1): `ProgramBlock.instr[i]` counts ctr `.1` of plan node `.0`
    /// planprod P1-b: frozen into `side.v4`, the program's own arena (`with_v4` / `set_v4`); declared before `side`, so the
    /// descriptors drop before their arena.
    pub v4: Option<crate::protos::FrozenV4>,
    /// The kinds that made it (lowerv4 mechs_used.rs, KindSet bits): the report's mechanisms line only. 0 when no v4
    /// lowering built it, and also for every v4-lowered program while the record is off (it is on only under
    /// PGRUST_GRAVITY_REPORT), so it never says whether a lowering built the program: `lowered` does (D4352). Not in the
    /// content key: a function of the lowering that built the key's IR.
    pub used: u128,
    /// D4352: `gravity_lower::assemble` built it (`as_lowered`), whatever the mechanisms record (`used`) holds.
    v4_lowered: bool,
    /// A LockRows over several locking marks (W5 multilock Row A): one lock per mark, in PG's lock order; empty for any
    /// other program (a lock's is `write`'s).
    pub locks: ArenaVec<LockSite>,
    /// D1687: the write plans beside `write` (writing CTEs beside the statement's write, or a second writing CTE), each by
    /// its write site; empty for a program of one write plan.
    pub writes: ArenaVec<WriteAt>,
    /// The arenas of the descriptors set after the build (`with_instr`, `with_locks`, `with_writes`), each sized exactly:
    /// the program's as `arena` is the build's, so `file_on_ledger` moves them with it. After every list in them.
    side: Side,
    /// D2605: where `write`'s ModifyTable ends (`WriteAt::end`); `END_STATEMENT` for every program but one whose `write` is
    /// a pulled data-modifying CTE's (two writing CTEs under a SELECT).
    pub write_end: u16,
    /// D3586: `front_key`, hashed at its first call. What it hashes (the layout, sites, output columns, write plan and
    /// pipelines) is fixed by then: the builders that take the program by value change none of it, and its one `&mut`
    /// edit (exec admit `serial_over_index_sites` / `serial_over_temp_sources`, after `assemble`) comes before the
    /// program is filed behind an `Arc` and run; a debug build checks each call against a fresh hash. Hashing every site
    /// and pipeline per execution was 15 % of a pruned generic EXECUTE over 2000 partitions.
    front: pgsync::OnceLock<ContentKey>,
}

/// `Program::side`: one arena per descriptor list set after the build (`None`: never set).
#[derive(Default)]
struct Side {
    instr: Option<Arena>,
    locks: Option<Arena>,
    /// D2644: `writes` and its plans' lists (indexes, arbiters, route leaves).
    writes: Option<Arena>,
    /// planprod P1-b: the v4 descriptors' frozen copy (`V4Descs::freeze`).
    v4: Option<Arena>,
}

/// `v` copied into an arena of its own, sized exactly: the arena and the list in it.
fn side_copy<T: Copy>(v: &[T]) -> (Arena, ArenaVec<T>) {
    let arena = Arena::new(round8(core::mem::size_of_val(v)).max(8));
    let list = copy_vec(v, arena.handle());
    (arena, list)
}

/// C2: a program filed in a process-wide cache holds its arenas on the program caches' memory account
/// (`Program::file_on_ledger`): their door-1 bytes on one Cache(ProgramCaches) charge (`heap`, C8), and their door-2
/// bytes on each mapped arena's own charge, which the filing moved to the program caches (`mapped` counts them). The drop
/// drops the door-1 charge, which moves those bytes back to the dropping thread's account, whose door 1 then takes them
/// off with the arenas (inside one drop the two steps' order changes no account's net); a mapped arena's drop takes its
/// pages off the program caches. A program never filed moves nothing.
#[derive(Default)]
struct Ledgered {
    filed: core::sync::atomic::AtomicBool,
    heap: pgsync::Mutex<Option<memledger::Charge>>,
    mapped: core::sync::atomic::AtomicU64,
}

unsafe impl Send for Program {}
unsafe impl Sync for Program {}

/// D1687: a write plan beside the program's `write` and the write site its row ops name (the driver's writer for it).
/// Never cloned: the lowering builds each once and `with_writes` copies it into the program's arena (`WritePlan::copy_in`).
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct WriteAt {
    pub site: u32,
    pub plan: WritePlan,
    /// D2605: where its ModifyTable ends (`END_STATEMENT`, or the program index of the pipeline whose pulled set it is).
    pub end: u16,
    /// D3623: its ModifyTable's plan node id (0: not known), which its writer finds its own modified columns by.
    pub node: i32,
}

/// D2605 (`WriteAt::end`, `Program::write_end`): a writer bound with its program at `apply`, whose AFTER STATEMENT events
/// queue at the statement's end in bind order. Any other value is the program index of a data-modifying CTE's pipeline,
/// pulled by its readers (lowerv4 ops/ctescan/lower.rs): PostgreSQL calls that CTE's ModifyTable first when its first
/// reader reads past the stored rows, and it ends when a reader, or ExecPostprocessPlan, reads past its last row. The
/// driver binds such a writer at its set's first pull (its BEFORE STATEMENT triggers) and queues its AFTER STATEMENT
/// events once the set ran to its end (driver node/ends.rs).
pub const END_STATEMENT: u16 = u16::MAX;

/// One locking PlanRowMark of a LockRows over several (ExecRowMark): its relation, strength and wait policy. The IR names
/// it in `lock_row`'s argument, `arg(site, i)`: the site whose position it locks, and its index in `Program.locks` + 1
/// above `SHIFT` (0 = the program's one lock, `write`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LockSite {
    pub rti: u32,
    pub relid: u32,
    pub lock: LockKind,
    pub wait: WaitPolicy,
}

impl LockSite {
    pub const SHIFT: u32 = 32;
    /// REFFETCH: `lock_row`'s argument flag for a REFERENCE-marked relation's row fetched at the site's position
    /// (EvalPlanQualFetchRowMark: the version the join read, under SnapshotAny) into the site's cells; nothing is locked.
    pub const FETCH: u64 = 1 << 63;

    pub const fn plan(&self) -> WritePlan {
        WritePlan::locking(self.relid, self.lock, self.wait)
    }

    pub const fn arg(site: usize, i: usize) -> u64 {
        site as u64 | ((i as u64 + 1) << LockSite::SHIFT)
    }
}

/// The last pipeline a lazy pipeline's work runs beside (D2495): `lazy_for`'s chain (a lazy build fed into another lazy
/// build) to the outermost probe; `i` itself when it is not lazy.
fn lazy_probe(ps: &[Pipeline], i: usize) -> usize {
    let mut q = i;
    while let Some(next) = usize::try_from(ps[q].lazy_for).ok().filter(|&next| next > q && next < ps.len()) {
        q = next;
    }
    q
}

fn last_consumer_in(ps: &[Pipeline], producer: usize) -> usize {
    let Some(slot) = ps[producer].produces() else { return producer };
    let next = (producer + 1..ps.len()).find(|&k| ps[k].produces() == Some(slot)).unwrap_or(ps.len());
    let held = usize::try_from(ps[producer].live_through).map_or(producer, |k| k.min(next.saturating_sub(1)));
    (producer + 1..next).filter(|&k| ps[k].consumes.contains(&slot)).max().unwrap_or(producer).max(held)
}

/// A pipeline without the planner's estimates, as the content key hashes it (lower inputs/key.rs): the rows, the build rows,
/// the row width, `end_rate` (NEW-48) and `end_rows`. A lowering's decision on one is a guard of its key, and the rest only
/// size (D3205, DR-1).
pub fn estimates_out(p: &Pipeline) -> Pipeline {
    Pipeline { predicted_rows: 0, build_rows: 0, row_width: 0, end_rate: 0, end_rows: 0, ..*p }
}

/// What `bind_key` hashes of a pipeline: `estimates_out`, but for the rows and build rows no `rows_of` / `build_of` names
/// (admission binds a named one per statement; an unnamed one is the lowering's own, D4002), and without the run-ahead
/// tracer's stamps `pg_gate` / `pg_regions` / `quiet` / `licence` / `gate_tags` (construct C2 (b): a diagnosis, never a decision; the content key leaves them out too),
/// nor the end's spill answer `holds` (C10: it follows from the code and the breaker, both keyed).
fn bound(p: &Pipeline) -> Pipeline {
    let own = |of: u16, v: u64| if of == u16::MAX { v } else { 0 };
    Pipeline { predicted_rows: own(p.rows_of, p.predicted_rows), build_rows: own(p.build_of, p.build_rows), pg_gate: -1, pg_regions: [-1; 4], quiet: false, licence: LicenceStamp::None, gate_tags: None, holds: None, ..estimates_out(p) }
}

impl Program {
    /// D4002: the first part `bind_key` hashes that differs between two programs, named (the progcache check's message).
    pub fn bind_diff(&self, o: &Program) -> String {
        macro_rules! first {
            ($what:expr, $a:expr, $b:expr) => {
                if let Some(i) = (0..$a.len().max($b.len())).find(|&i| $a.get(i) != $b.get(i)) {
                    return format!("{} {i} ({:?} / {:?})", $what, $a.get(i), $b.get(i)); // alloc: Cold
                }
            };
        }
        if self.layout != o.layout {
            return format!("the layout ({:?} / {:?})", self.layout, o.layout); // alloc: Cold
        }
        first!("site", self.sites, o.sites);
        first!("output column", self.out, o.out);
        if self.write != o.write {
            return format!("the write ({:?} / {:?})", self.write, o.write); // alloc: Cold
        }
        if self.pipelines.len() != o.pipelines.len() {
            return format!("{} / {} pipelines", self.pipelines.len(), o.pipelines.len()); // alloc: Cold
        }
        for (i, (a, b)) in self.pipelines.iter().zip(o.pipelines.iter()).enumerate() {
            let (a, b) = (bound(a), bound(b));
            let Pipeline { driving_site, ends_in, parallel, predicted_rows, consumes, consumes_from, claims, owner, admit, words, topk, finish,
                store_row_bytes, build_rows, recheck_skip, lazy_for, recheck_hold, recheck_hold_epq, held_sort, held_at, recheck, whole_read,
                ahead, live_through, rows_of, build_of, source_of, row_width, post, first_call, end_rate, end_rows, end_of, top_rows, pg_gate, pg_regions, quiet, pass_of, licence, gate_tags, holds, refills, cascade } = b;
            macro_rules! field {
                ($($f:ident),*) => { $( if a.$f != $f { return format!("pipeline {i}'s {} ({:?} / {:?})", stringify!($f), a.$f, $f); } )* }; // alloc: Cold
            }
            field!(driving_site, ends_in, parallel, predicted_rows, consumes, consumes_from, claims, owner, admit, words, topk, finish, store_row_bytes,
                build_rows, recheck_skip, lazy_for, recheck_hold, recheck_hold_epq, held_sort, held_at, recheck, whole_read, ahead, live_through, rows_of,
                build_of, source_of, row_width, post, first_call, end_rate, end_rows, end_of, top_rows, pg_gate, pg_regions, quiet, pass_of, licence, gate_tags, holds, refills, cascade);
            if self.codes[i].identity() != o.codes[i].identity() {
                return format!("pipeline {i}'s code ({:?} / {:?})", self.codes[i].identity(), o.codes[i].identity()); // alloc: Cold
            }
        }
        if self.out_region != o.out_region {
            return format!("the out region ({} / {})", self.out_region, o.out_region); // alloc: Cold
        }
        first!("instrumented cell", self.instr, o.instr);
        first!("lock", self.locks, o.locks);
        first!("beside write", self.writes, o.writes);
        if self.write_end != o.write_end {
            return format!("the write end ({} / {})", self.write_end, o.write_end); // alloc: Cold
        }
        "the owner's signature (`with_owner`)".to_string() // alloc: Cold
    }

    pub fn new(layout: Layout, sites: &[SiteDesc], out: &[OutCol], write: WritePlan, code: Code) -> Program {
        let driving = sites.iter().position(SiteDesc::is_source).map_or(-1, |i| i as i16);
        let reason = if driving < 0 { Reason::NoDrivingSite } else if sites[driving as usize].is_range() { Reason::Sink } else { Reason::IndexSource };
        Program::with_pipelines(layout, sites, out, write, [(Pipeline::serial(driving, End::Sink, reason), code)])
    }

    /// The out block's by-reference region (lower.md §15.13): `bytes` behind every node's out block; the content key folds it in. `out_region_bytes` is the lowerer's size.
    /// `sig` names what the owner binds (the hosted sites' functions): two programs of one IR and layout over
    /// different functions are different programs, so the content key folds it in.
    pub fn with_owner(mut self, owner: std::rc::Rc<dyn core::any::Any>, sig: &[u32]) -> Program {
        self.fold(|h| sig.hash(h));
        self.owner = Some(owner);
        self
    }

    /// Re-keys `key` and `bind_key` alike with what `f` hashes.
    fn fold(&mut self, f: impl Fn(&mut Fnv128)) {
        for k in [&mut self.key, &mut self.bind_key] {
            let mut h = Fnv128 { lo: k.0 as u64, hi: (k.0 >> 64) as u64 };
            f(&mut h);
            *k = ContentKey(((h.hi as u128) << 64) | h.lo as u128);
        }
    }

    /// planprod P1-b: the arena `set_v4` froze the descriptors into; the old one (if any) drops after the old descriptors.
    pub(crate) fn set_v4_arena(&mut self, a: Arena) {
        self.side.v4 = Some(a);
    }

    #[cfg(test)]
    pub(crate) fn v4_arena(&self) -> Option<&Arena> {
        self.side.v4.as_ref()
    }

    /// The instrumented program's counter cells (`instr`), folded into the key: it never answers for the plain program.
    pub fn with_instr(mut self, cells: &[(i32, u8)]) -> Program {
        self.fold(|h| (0x1257u32, cells).hash(h));
        let (arena, list) = side_copy(cells);
        self.instr = list;
        self.side.instr = Some(arena);
        self
    }

    /// C10: a v4 lowering built this program (`as_lowered`; a hand program that records kinds, `used`, counts too), so
    /// its breakers' spill answers are the lowered ends' (`Pipeline::holds`); a hand program declares its own ends.
    /// D4352: not `used` alone: the record is off outside PGRUST_GRAVITY_REPORT, so every served program read as a hand
    /// program, no region had a spill road, and revoke found no spillable bytes to ask for.
    pub const fn lowered(&self) -> bool {
        self.v4_lowered || self.used != 0
    }

    /// D4352: a v4 lowering built this program (`gravity_lower::assemble`).
    pub fn as_lowered(mut self) -> Program {
        self.v4_lowered = true;
        self
    }

    /// The mechanisms-used record (`used`), never folded into the key.
    pub fn with_used(mut self, used: u128) -> Program {
        self.used = used;
        self
    }

    /// A LockRows' several locks (`locks`), folded into the key; none leaves the program (and its key) as it was.
    pub fn with_locks(mut self, locks: &[LockSite]) -> Program {
        if locks.is_empty() {
            return self;
        }
        self.fold(|h| (0x10c5u32, locks).hash(h));
        let (arena, list) = side_copy(locks);
        self.locks = list;
        self.side.locks = Some(arena);
        self
    }

    /// D1687: the write plans beside `write` (`writes`), folded into the key; none leaves the program (and its key) as it was.
    /// D2644: copied into the program's own arena, sized exactly, as `write` is: the caller's lists live in the lowering's
    /// arena, which admission resets before the program runs (a writing CTE's ON CONFLICT arbiters read back as zeros).
    pub fn with_writes(mut self, writes: &[WriteAt]) -> Program {
        if writes.is_empty() {
            return self;
        }
        self.fold(|h| (0x1687u32, writes).hash(h));
        let bytes = round8(core::mem::size_of_val(writes)) + writes.iter().map(|w| w.plan.arena_bytes()).sum::<usize>();
        let arena = Arena::new(bytes); // alloc: Cold
        let a = arena.handle();
        let mut list = ArenaVec::with_capacity_in(writes.len(), a);
        for w in writes {
            list.push(WriteAt { site: w.site, plan: w.plan.copy_in(a), end: w.end, node: w.node });
        }
        assert!(a.used() <= bytes, "beside write plans' arena: {} of {bytes}", a.used());
        self.writes = list;
        self.side.writes = Some(arena);
        self
    }

    /// D2605: where `write`'s ModifyTable ends (`write_end`), folded into the key; `END_STATEMENT` leaves both as they were.
    pub fn with_write_end(mut self, end: u16) -> Program {
        if end == END_STATEMENT {
            return self;
        }
        self.fold(|h| (0x2605u32, end).hash(h));
        self.write_end = end;
        self
    }

    pub fn with_out_region(mut self, bytes: u32) -> Program {
        self.fold(|h| bytes.hash(h));
        self.out_region = bytes;
        self
    }

    /// The program's one allocation: its arena, sized exactly from the descriptors.
    pub fn with_pipelines(layout: Layout, sites: &[SiteDesc], out: &[OutCol], write: WritePlan, pipelines: impl IntoIterator<Item = (Pipeline, Code)>) -> Program {
        Program::build(layout, sites, out, write, pipelines, true)
    }

    /// A lowered program (`gravity_lower::assemble`): lowerv4 declares every slot a pipeline reads, so no sink takes the
    /// nearest earlier producer's slot by default; a sink after an on-demand run (an Init's members, a LAZY set's build:
    /// M8) never reads its slot, and the run stays on demand (D1114)
    pub fn with_declared_pipelines(layout: Layout, sites: &[SiteDesc], out: &[OutCol], write: WritePlan, pipelines: impl IntoIterator<Item = (Pipeline, Code)>) -> Program {
        Program::build(layout, sites, out, write, pipelines, false)
    }

    fn build(layout: Layout, sites: &[SiteDesc], out: &[OutCol], write: WritePlan, pipelines: impl IntoIterator<Item = (Pipeline, Code)>, chain: bool) -> Program {
        assert_eq!(layout.nsites as usize, sites.len(), "layout.nsites vs site descriptors");
        assert_eq!(layout.out_natts as usize, out.len(), "layout.out_natts vs output columns");
        let mut h = Fnv128 { lo: 0xcbf2_9ce4_8422_2325, hi: 0x84222325_cbf29ce4 };
        layout.hash(&mut h);
        sites.hash(&mut h);
        out.hash(&mut h);
        write.hash(&mut h);
        let staged: Vec<(Pipeline, Code)> = pipelines.into_iter().collect(); // alloc: Cold
        let n = staged.len();
        let mut b = Fnv128 { lo: h.lo, hi: h.hi };
        for (p, code) in staged.iter() {
            // construct C2 (b): the run-ahead tracer's stamps are a diagnosis, never a decision: two lowerings that differ
            // in them run alike. C10: a lowered end's spill answer (`holds`) follows from its code and breaker, both keyed
            let p = &Pipeline { pg_gate: -1, pg_regions: [-1; 4], quiet: false, licence: LicenceStamp::None, gate_tags: None, holds: None, ..*p };
            p.hash(&mut h);
            code.identity().hash(&mut h);
            bound(p).hash(&mut b);
            code.identity().hash(&mut b);
        }
        assert!(n > 0, "a program has a pipeline");
        let key = ContentKey(((h.hi as u128) << 64) | h.lo as u128);
        let bind_key = ContentKey(((b.hi as u128) << 64) | b.lo as u128);
        let nslots = staged.iter().filter_map(|(p, _)| p.produces()).map(|s| s as usize + 1).max().unwrap_or(0);
        let words = crate::laneset::words_for(nslots);
        let bytes = round8(sites.len() * core::mem::size_of::<SiteDesc>())
            + sites.iter().map(SiteDesc::arena_bytes).sum::<usize>()
            + round8(out.len() * core::mem::size_of::<OutCol>())
            + round8(n * core::mem::size_of::<Pipeline>())
            + round8(n * core::mem::size_of::<Code>())
            + write.arena_bytes()
            + round8(nslots * 4)
            + round8(nslots)
            + round8(nslots * 2)
            + words * n * 8
            + staged.iter().filter(|(p, _)| p.consumes.len() > 1).map(|(p, _)| round8(p.consumes.len())).sum::<usize>()
            + staged.iter().map(|(p, _)| round8(p.consumes_from.len() * core::mem::size_of::<LifeId>())).sum::<usize>()
            + staged.iter().filter_map(|(p, _)| p.owner.as_ref()).map(|o| round8(o.check.steps.len() * core::mem::size_of::<CheckStep>())).sum::<usize>();
        let arena = Arena::new(bytes); // alloc: Cold
        let a = arena.handle();
        let mut s = ArenaVec::with_capacity_in(sites.len(), a);
        for d in sites {
            s.push(d.copy_in(a));
        }
        let o = copy_vec(out, a);
        let write = write.copy_in(a);
        let mut ps = ArenaVec::with_capacity_in(n, a);
        let mut codes = ArenaVec::with_capacity_in(n, a);
        for (mut p, code) in staged {
            if p.consumes.len() > 1 {
                let mut v: ArenaVec<u8> = ArenaVec::with_capacity_in(p.consumes.len(), a);
                v.extend_from_slice(p.consumes);
                // SAFETY: the program's arena outlives its pipelines.
                p.consumes = unsafe { &*(v.leak() as *const [u8]) };
            }
            if !p.consumes_from.is_empty() {
                let mut v: ArenaVec<LifeId> = ArenaVec::with_capacity_in(p.consumes_from.len(), a);
                v.extend_from_slice(p.consumes_from);
                // SAFETY: the program's arena outlives its pipelines.
                p.consumes_from = unsafe { &*(v.leak() as *const [LifeId]) };
            }
            if let Some(o) = p.owner.as_mut().filter(|o| !o.check.steps.is_empty()) {
                o.check = o.check.copy_in(a);
            }
            ps.push(p);
            codes.push(code);
        }
        let last = ps.len() - 1;
        for (i, p) in ps.iter().enumerate() {
            assert!(p.driving_site < sites.len() as i16, "pipeline {i} driving site");
            assert!(p.driving_site < 0 || sites[p.driving_site as usize].is_source(), "pipeline {i} drives a non-source site");
            // the last pipeline ends in the sink, or in a `Materialize` breaker: a scratch region per worker whose rows go to the sink (the partitioned join's third phase)
            assert!(i < last || p.produces().is_none() || matches!(p.ends_in, End::Breaker(Breaker { kind: gravity_abi::breaker::BreakerKind::Materialize, .. })), "the last pipeline must end in the sink");
            if let Some(d) = p.admit {
                // the two-pass protocol (§2.6 item 15): an earlier HashAgg producer this pass consumes, both with an owner phase
                let p1 = ps.get(d.pass1 as usize).filter(|_| (d.pass1 as usize) < i).unwrap_or_else(|| panic!("pipeline {i} admits from pipeline {}", d.pass1));
                let slot = match p1.ends_in {
                    End::Breaker(Breaker { kind: gravity_abi::breaker::BreakerKind::HashAgg, slot, .. }) => slot,
                    _ => panic!("pipeline {i}'s pass 1 must produce a HashAgg"),
                };
                assert!(p.consumes.contains(&slot), "pipeline {i} consumes its pass 1's slot");
                assert!(p1.owner.is_some(), "pipeline {i}'s pass 1 declares its owner phase");
            }
        }
        // a kernel word is a u16 slot (`WordsDesc`): one owner per slot, over every slot the statement declares. A pipeline
        // declares at most five, so each is looked up in the earlier pipelines' own declarations rather than in a table
        // the size of the highest slot.
        let words_of = |p: &Pipeline| p.words.map_or([WordsDesc::NONE; 5], |d| d.slots());
        for (i, p) in ps.iter().enumerate() {
            for s in words_of(p).into_iter().filter(|&s| s != WordsDesc::NONE) {
                let o = ps[..i].iter().position(|q| words_of(q).contains(&s));
                assert!(o.is_none(), "pipeline {i} declares a kernel word another pipeline declares (its words {:?}, word {s} taken by pipeline {})", p.words, o.unwrap_or(usize::MAX));
            }
        }
        let slots = Program::validate_dag(&mut ps, nslots, a, chain);
        let used = a.used();
        assert!(used <= bytes, "program arena: {used} of {bytes}");
        Program { key, bind_key, layout, sites: s, out: o, write, pipelines: ps, slots, out_region: 0, codes, arena, owner: None, hosted: None, instr: ArenaVec::new_in(ArenaRef::EMPTY), v4: None, used: 0, v4_lowered: false, locks: ArenaVec::new_in(ArenaRef::EMPTY), writes: ArenaVec::new_in(ArenaRef::EMPTY), side: Side::default(), write_end: END_STATEMENT, front: pgsync::OnceLock::new(), ledger: Ledgered::default() }
    }

    /// The DAG rules (§2 decision 4): a pipeline never consumes what it produces; every consumed slot was produced by an earlier pipeline (the nearest producer of it: so the program order is a topological order and the graph acyclic); producing a slot again ends the earlier
    /// production's lifetime (its consumers are the ones before the new producer). A sink pipeline declaring nothing consumes the nearest earlier producer's slot (`chain`: not a lowered program's). `nslots` is the highest produced slot plus one.
    fn validate_dag(ps: &mut ArenaVec<Pipeline>, nslots: usize, a: ArenaRef, chain: bool) -> SlotPlan {
        let n = ps.len();
        let mut plan = SlotPlan::new_in(a, nslots, n);
        // slot ids are u8: one flag per id
        let mut produced = [false; 1 << u8::BITS];
        let mut last_producer: Option<u8> = None;
        for i in 0..n {
            let mut p = ps[i];
            if chain && p.produces().is_none() && p.nconsumed() == 0 {
                if let Some(s) = last_producer {
                    p = p.consuming_one(s);
                    ps[i] = p;
                }
            }
            assert!(p.consumes_from.is_empty() || p.consumes_from.len() == p.consumes.len(), "pipeline {i} names the life of some consumed slots only");
            for (k, c) in p.consumes.iter().enumerate() {
                let c = *c as usize;
                assert!(p.produces() != Some(c as u8), "pipeline {i} consumes the slot it produces");
                assert!(c < nslots && produced[c], "pipeline {i} consumes slot {c} before any pipeline produced it");
                assert!(!p.consumes[..k].contains(&(c as u8)), "pipeline {i} consumes one slot twice");
                if let Some(&LifeId(l)) = p.consumes_from.get(k) {
                    let l = l as usize;
                    assert!(l < i && ps[l].produces() == Some(c as u8), "pipeline {i} reads slot {c} from life {l}, which does not produce it");
                    assert!((l + 1..i).all(|m| ps[m].produces() != Some(c as u8)), "pipeline {i} reads life {l} of slot {c} past the slot's next producer");
                }
            }
            if let Some(b) = p.breaker() {
                let s = b.slot as usize;
                produced[s] = true;
                match b.budget {
                    Budget::Fixed(bytes) => plan.bytes[s] = plan.bytes[s].max(bytes),
                    Budget::Share | Budget::Pool => plan.shared[s] = true,
                }
                last_producer = Some(b.slot);
            }
        }
        // lifetimes: a slot is live from its producer through its last consumer before the slot's next producer; D2495: a
        // lazy build's pipelines (`lazy_for` = q, the build and the producers fed into it) run at q's first row, beside the
        // slots q opens, so their slots are live through q, and through q's own probe when q is itself lazy
        let words = plan.words;
        for i in 0..n {
            if let Some(s) = ps[i].produces() {
                let end = last_consumer_in(ps, i).max(lazy_probe(ps, i));
                for p in i..=end {
                    crate::laneset::set(&mut plan.live[p * words..(p + 1) * words], s as usize);
                }
            }
        }
        plan.max_live = (0..n).map(|p| crate::laneset::count(plan.live_words(p)) as u16).max().unwrap_or(0);
        // D3034: a share breaker's divisor counts the share slots live beside it, never a program-constant one
        // (`Budget::Fixed`: its region is its own bytes, no part of the grant). A count-ordered top's fixed owner slot live
        // beside its group table halved the table's share: a spill and a slow statement end.
        for p in 0..n {
            let count = (0..nslots).filter(|&s| plan.shared[s] && plan.is_live(p, s as u8)).count().max(1) as u16;
            for s in 0..nslots {
                if plan.is_live(p, s as u8) {
                    plan.peak[s] = plan.peak[s].max(count);
                }
            }
        }
        plan
    }

    pub fn arena_used(&self) -> usize {
        self.arena.handle().used()
    }

    /// C2: this program, filed in a process-wide cache (exec admit's shared tier), moves its arenas' bytes (the build's and
    /// the lists set after it, `side`) from the filing thread's memory account onto the program caches' account, once; they leave it at its drop (`Ledgered`; a mapped
    /// arena's pages with the arena's charge), on whichever thread drops it last, so a program evicted while a backend still
    /// runs it stays on the account until then.
    /// Its code is on that account already (the code arena's door 2, elf-mapper). Call it on the thread that built it.
    pub fn file_on_ledger(self: &std::sync::Arc<Self>) {
        use core::sync::atomic::Ordering::Relaxed;
        if self.ledger.filed.swap(true, Relaxed) {
            return;
        }
        let caches = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::ProgramCaches));
        let mut heap = memledger::arc_bytes::<Program>() + self.v4.as_ref().map_or(0, |_| memledger::arc_bytes::<crate::protos::V4Descs<ArenaRef>>());
        let side = [self.side.instr.as_ref(), self.side.locks.as_ref(), self.side.writes.as_ref(), self.side.v4.as_ref()];
        for a in core::iter::once(&self.arena).chain(side.into_iter().flatten()) {
            match a.ledger_bytes() {
                (bytes, true) => {
                    // C8 / D4518: the block goes with its own charge (a mapping's pages, a boxed block whole), so the drop
                    // takes it off the caches on any thread
                    a.transfer_charge(caches, memledger::Release::Evict);
                    self.ledger.mapped.fetch_add(bytes, Relaxed);
                }
                (bytes, false) => heap += bytes,
            }
        }
        if heap > 0 {
            // C8: the boxed arenas' bytes with no charge of their own (wasm) and the program's own blocks (D4580), which this
            // thread's door 1 counted, move to the program caches' charge
            let c = memledger::Charge::open(caches, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, heap);
            *self.ledger.heap.lock().unwrap_or_else(|e| e.into_inner()) = Some(c);
        }
    }

    /// C2: the bytes this program holds on the program caches' memory account (0: never filed).
    pub fn ledgered_bytes(&self) -> u64 {
        use core::sync::atomic::Ordering::Relaxed;
        let heap = self.ledger.heap.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(0, memledger::Charge::committed);
        heap + self.ledger.mapped.load(Relaxed)
    }

    /// The L2 front key (milestone-2.md §2d, §4 item 1): the program's content without its code — layout, sites, output columns, write plan and pipelines. Two programs that differ only in
    /// the code that runs them (a hand kernel, a lowered function) share it; `ProgramCache::install` files a program under it and the admission takes that program for a matching shape.
    pub fn front_key(&self) -> ContentKey {
        let key = *self.front.get_or_init(|| self.front_hash());
        debug_assert_eq!(key, self.front_hash(), "gravity: a program's front content changed after its first front_key");
        key
    }

    fn front_hash(&self) -> ContentKey {
        let mut h = Fnv128 { lo: 0xcbf2_9ce4_8422_2325, hi: 0x84222325_cbf29ce4 };
        self.layout.hash(&mut h);
        self.sites[..].hash(&mut h);
        self.out[..].hash(&mut h);
        self.write.hash(&mut h);
        self.pipelines[..].hash(&mut h);
        ContentKey(((h.hi as u128) << 64) | h.lo as u128)
    }

    pub fn code(&self) -> &Code {
        &self.codes[0]
    }

    pub fn code_of(&self, pipeline: usize) -> &Code {
        &self.codes[pipeline]
    }

    /// The machine code the program holds: each mapping once (D3615: one compile maps every pipeline's entry, and each of
    /// those codes reports the whole mapping's bytes; summed per pipeline, a 101-pipeline program read as 17 MB of code
    /// for a 170 KB mapping, past the process-wide tier's shard budget, so it was never filed).
    /// A mapping counts at its first code; the codes are one per pipeline, so the earlier ones are read again rather than
    /// kept in a list.
    pub fn code_bytes(&self) -> usize {
        let codes = &self.codes[..];
        codes
            .iter()
            .enumerate()
            .filter(|&(i, c)| match c.mapping() {
                0 => true,
                m => !codes[..i].iter().any(|d| d.mapping() == m),
            })
            .map(|(_, c)| c.bytes())
            .sum()
    }

    /// D4579: the code mappings this program's codes run from (`Code::mapping`, the code cache module's address; a
    /// mapping several codes share, once per code), none for code of its own: what a tier removal asks the code cache to
    /// let go once nothing else holds it.
    pub fn code_mappings(&self) -> impl Iterator<Item = usize> + '_ {
        self.codes.iter().map(Code::mapping).filter(|&m| m != 0)
    }

    /// D4762: the code cache keys of the modules this program's tier-0 codes promote from (`Code::module`).
    pub fn code_modules(&self) -> impl Iterator<Item = u128> + '_ {
        self.codes.iter().map(Code::module).filter(|&k| k != 0)
    }

    pub fn nsources(&self) -> usize {
        self.sites.iter().filter(|d| d.is_source()).count()
    }

    /// The pipeline whose breaker fills `slot` for consumer `pipeline` (the nearest earlier producer of it).
    pub fn producer_of_slot(&self, pipeline: usize, slot: u8) -> Option<usize> {
        (0..pipeline).rev().find(|&p| self.pipelines[p].produces() == Some(slot))
    }

    /// The pipelines `pipeline` waits for: the producers of every slot it consumes, then those it follows (v4
    /// `ProtoKind::Follows`: an order edge, no slot; M7-DRAIN V4R-377).
    pub fn producers_of(&self, pipeline: usize) -> impl Iterator<Item = usize> + '_ {
        self.pipelines[pipeline].consumes.iter().filter_map(move |&s| self.producer_of_slot(pipeline, s)).chain(self.v4.iter().flat_map(move |d| d.follows(pipeline)))
    }

    /// The last pipeline that reads the slot pipeline `producer` produces, before the slot's next producer (`producer` itself when none).
    pub fn last_consumer(&self, producer: usize) -> usize {
        last_consumer_in(&self.pipelines, producer)
    }

    /// A worker's partial region for the program-constant breakers (the largest of them).
    pub fn worker_partial_bytes(&self) -> u32 {
        self.slots.fixed_max()
    }
}
