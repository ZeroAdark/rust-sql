//! M6's hosted aggregate bracket (M6-plan.md §6, cards/M6.md L2 `hosts/src/v4/m6.rs`, V4R-269's `agg.arena`; a
//! `#[path]` submodule of hosts v4/m6.rs): `AggArena`, v1 `aggstate.rs`'s `GenericAgg` over `types_fmgr::agg::AggStateNode`,
//! reached by the kernels entries (host_abi/v4/m6_agg.rs) through its head. Created by M6-HOSTS.
use core::hash::{BuildHasherDefault, Hasher};
use core::sync::atomic::{AtomicU64, Ordering};
use std::collections::{HashMap, HashSet};

use datum::{Datum, NullableDatum};
use gravity_abi::hash::phi_fold;
use gravity_kernels::ops::agg::trans::hosted::{AggHead, AGG_MAGIC, AGG_STATE_BYTES, DIRECT_BOUND, DIRECT_NULL, DIRECT_VALUE, STATE_AGGNO, STATE_INIT, STATE_NOTRANS, STATE_NULL, STATE_VALUE};
use mcx::MemoryContext;
use memledger::charge::Door;
use memledger::{Charge, Kind, Owner};
use pgsync::Mutex;
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_fmgr::AggStateNode;
use types_nodes::primnodes::Aggref;

/// D1323: `STATE_INIT` of a DISTINCT state whose run `fold_once` folded (its value, NULL and no-trans bytes the folded state's).
pub(crate) const FOLDED: u8 = 2;
/// D1520 / D4040: `STATE_INIT` of a finalized 'w' state, or an unshared 's' one (its sort has no random access to rescan):
/// its value and NULL bytes are the final's result, returned by every later final of the group (an emit's order pass, a
/// top's drain, a resumed row), as PG finalizes a group once.
const MEMO: u8 = 3;
const AGGMODIFY_READ_WRITE: i8 = b'w' as i8;
const AGGMODIFY_SHAREABLE: i8 = b's' as i8;

mod raw;

pub(crate) use crate::ops::agg::trans::hosted_bind::{Calls, Spec};
use crate::ops::agg::raw::read_only;
use crate::ops::agg::trans::hosted_kept::image_len;

/// One worker slot: its calls and its aggregate context, owner-exclusive: a worker ordinal's (a pool lease runs one
/// thread at a time on it) or, last, the leader's merge slot. A parallel arena's roots are unobserved (mcx
/// `new_unobserved`: no weak entry in a thread's context forest), so a slot is made, used and dropped on any thread in
/// turn; a serial arena's are the leader's own.
pub(crate) struct Slot {
    pub(crate) aggs: Vec<Calls>, // alloc: Exec
    pub(crate) node: Box<AggStateNode>, // alloc: Exec
    pub(crate) tmp: MemoryContext,
    pub(crate) calls: u32,
    /// The aggcontext's bytes, refreshed at each final and every 256 transitions.
    pub(crate) bytes: AtomicU64,
    /// D3267: the mapped road the slot's DISTINCT runs take past `AggArena::budget`, dropped with the aggcontext's reset.
    pub(crate) spill: crate::ops::shared::mapped::Road,
}

/// The statement's aggregate arena (`agg.arena`): every group's by-reference state lives in its slot's aggcontext; the
/// slots are freed (each node's shutdown callbacks first) when the execution drops it, on every path. Parallel when
/// every aggregate has a combinefn and is `PARALLEL SAFE` (PG's rule, Decision 12): one slot per worker ordinal and the
/// merge slot, the partial states combined by `combine` (PG's `advance_combine_function`); else one slot at width 1.
#[repr(C)]
pub struct AggArena {
    head: AggHead,
    /// CONVERT (wave 2, H1): `gathered`'s table on `owner`, by its capacity (`table_bytes`); before it: it drops first.
    gathered_charge: Charge,
    pub(crate) specs: Vec<Spec>, // alloc: Exec
    pub(crate) slots: Vec<Slot>, // alloc: Exec
    parallel: bool,
    keep: MemoryContext,
    /// D345: the kept columns' (typlen, byval), aggno `naggs + i` (m6_agg_kept.rs).
    pub(crate) kept: Vec<(i16, bool)>, // alloc: Exec
    pub(crate) naggs: u32,
    /// V4R-375: the keyed groups' homes (kernels `m6_agg_home`), in `HOME_SHARDS` locked shards by the identity words: a
    /// wide arena's workers open homes at once. pglserve (D4601): a shard is made at its first home (`AggArena::shard`);
    /// an unmade one holds no home, no bytes and no charge, so the shards' readers walk the made ones (`AggArena::shards`).
    homes: Box<[pgsync::OnceLock<Mutex<Homes>>]>, // alloc: Exec
    /// D3062-b: the wide homes' gathered states (the last state of a home's aggregate, combined once; the emit's alone).
    gathered: HashSet<usize>, // alloc: Exec charged AggArena::owner (gathered_charged)
    /// CONVERT (wave 2, H1): the keyed fold's breaker slot owner and its ledger kind, read once from the first home's ctx
    /// (`raw::home_of`, `OpArena::slot_owner_kind`): it holds the homes' heap. None: no owner (the slot has none, or no
    /// host): nothing is charged, as before.
    owner: pgsync::OnceLock<Option<(Owner, Kind)>>,
    /// V4R-453 (D1409): a parallel arena's catalog facts, armed on each pool claim (`arm_catalog`). Declared after
    /// `slots`, so it drops after them: an armed typcache entry a slot's FmgrInfo kept (fn_extra) never outlives its image.
    catalog: Option<Box<crate::ops::agg::trans::hosted_catalog::CatalogArm>>, // alloc: Exec
    /// D2332: a plain Agg's arena (`plain_fed`): each aggregate's last by-reference final image, overwritten by its next
    /// final (the one group of an open; its emit copied the previous one out), so a rerun set's re-opens reuse it; D3578:
    /// a streamed group's (`streamed`), overwritten by the next group's.
    finals: Vec<Vec<u64>>, // alloc: Exec
    /// A streamed sorted group's arena (`streamed`): one open group at a time, so the first transition after a final
    /// starts the next group (`handed`) and resets the slot's aggregate context first (`next_group`).
    streamed: bool,
    handed: bool,
    /// D3267: work_mem in bytes when the arena was made (PG's per-aggregate sort ceiling, each participant its own): a
    /// slot whose aggcontext holds more puts its DISTINCT runs' later chunks on its mapped road (`Slot::spill`).
    pub(crate) budget: u64,
}

/// One shard of the keyed groups' homes: each group's one home (its record identity words -> its state slots), the homes
/// a fresh table's run forgot (kept to the arena's end: their records' tags never dangle), and their bytes (the grant's
/// charge: every state of a wide home).
/// CONVERT (wave 2, H1): the shard's heap (the states, the keys, the map's and the forgotten list's tables, by their
/// capacity) is charged to the keyed fold's slot owner (`AggArena::owner`), committed by the arena's batch
/// (`AggArena::homes_batch`) and at `AggArena::bytes` (each pipeline's end, the leader alone), so during a fold the shards
/// together wait uncharged at most what the folding threads' own ledger folds may (`memledger::DOOR_FOLD_BYTES` each). `charge` is declared first: it drops (its bytes back to the dropping thread's account) before the boxes it holds.
pub(crate) struct Homes {
    charge: Charge,
    pending: u64,
    /// The map's and the forgotten list's table bytes counted so far (`table_bytes`).
    tables: u64,
    map: HashMap<Box<[u64]>, Box<[u64]>, BuildHasherDefault<Words>>, // alloc: Exec charged AggArena::owner (Homes::note)
    forgot: Vec<Box<[u64]>>, // alloc: Exec charged AggArena::owner (Homes::note)
    bytes: u64,
}

impl Default for Homes {
    fn default() -> Homes {
        Homes { charge: Charge::NONE, pending: 0, tables: 0, map: HashMap::default(), forgot: Vec::new(), bytes: 0 } // alloc: Exec
    }
}

/// The heap of a hash table whose `capacity()` is `cap`, of `entry`-byte entries: hashbrown's buckets (capacity 3 or 7: 4 or
/// 8; past that `cap / 7 * 8`), each an entry and a control byte (the control word's group-width tail aside).
fn table_bytes(cap: usize, entry: usize) -> u64 {
    let buckets = match cap {
        0 => 0,
        1..=7 => cap + 1,
        _ => cap / 7 * 8,
    };
    (buckets * (entry + 1)) as u64
}

impl Homes {
    /// `heap` more bytes allocated for this shard (a new home's state and key) and its tables re-read: on its charge, opened
    /// on `owner` at the first (None: nothing is charged); committed when `batch` bytes wait (`AggArena::homes_batch`).
    fn note(&mut self, owner: Option<(Owner, Kind)>, heap: u64, batch: u64) {
        let tables = table_bytes(self.map.capacity(), core::mem::size_of::<(Box<[u64]>, Box<[u64]>)>()) + 16 * self.forgot.capacity() as u64;
        self.pending += heap + tables.saturating_sub(self.tables);
        self.tables = tables;
        if self.pending >= batch {
            self.commit(owner);
        }
    }

    /// The waiting bytes committed to `owner` (the charge opened at the first commit).
    fn commit(&mut self, owner: Option<(Owner, Kind)>) {
        let n = core::mem::take(&mut self.pending);
        if let Some(o) = owner.filter(|_| n != 0) {
            open_on(&mut self.charge, o);
            self.charge.commit(n);
        }
    }
}

/// `c` opened on `o` under kind `k` (the slot's breaker kind) with `o`'s own release (`release_of`, as wave2-base's scratch
/// arenas) before its first commit (a `Charge::NONE` charges no one).
fn open_on(c: &mut Charge, (o, k): (Owner, Kind)) {
    if c.committed() == 0 && c.owner() != o {
        *c = Charge::open(o, memledger::charge::release_of(o), k, Door::Heap, 0);
    }
}

/// D3062-b: the home shards (a power of two): 8 workers opening 15M groups' homes contended on one lock.
const HOME_SHARDS: usize = 64;

/// pglserve (D4601): an arena's home shard cells, each made at its first home; `PGRUST_GRAVITY_SERVE_LEAN=0` makes every
/// shard here, as before (a keyed arena of one group made and dropped 64 locked shards, and each pipeline end's
/// `AggArena::bytes` locked and committed all 64: ~10k instructions a statement of an `IN (SELECT .. LIMIT 1)`).
/// Charges: none moves. The eager 64 were never charged (`Homes::default` allocates nothing and its charge is
/// `Charge::NONE`); a shard, made or lazy, lives in place in this one box (no allocation of its own), and its map and
/// forgotten list are charged by `Homes::note` on the homes' owner from its first home, as before; `bytes` counts every
/// made shard, and an unmade one holds 0 bytes, as an untouched eager shard did.
fn home_cells() -> Box<[pgsync::OnceLock<Mutex<Homes>>]> {
    let cells = empty_home_cells();
    if !gravity_driver::env::serve_lean() {
        cells.iter().for_each(|c| {
            let _ = c.set(Mutex::default());
        });
    }
    cells
}

/// `home_cells`' one collect: the cells empty, each written in place (its state word only). The boxed slice is named by
/// the return type alone (a type named on the collect's line is one more alloclint `type Box` site). A cell built aside
/// and returned (a closure making it) is moved in whole, its unset payload too: rtt4 +2.5 kinstr a statement (AB8).
fn empty_home_cells() -> Box<[pgsync::OnceLock<Mutex<Homes>>]> {
    (0..HOME_SHARDS).map(|_| pgsync::OnceLock::new()).collect() // alloc: Exec
}

/// A group's home shard: the top bits of its identity words' `phi_fold`, another multiplier than `Words`', so a shard's
/// keys still spread over the map's buckets.
fn home_shard(keys: &[u64]) -> usize {
    (phi_fold(keys) >> 58) as usize & (HOME_SHARDS - 1)
}

/// The home map's hasher over T1's key images (integers, fingerprints, tagged bytes): the words mixed a word at a time by
/// a multiply, and `finish` rotates the product's well-mixed high bits down to the low bits the map's buckets read. The
/// multiply alone leaves the low bits a function of the words' low bits only: a tagged text image (`key.tagged`, its
/// bytes as words) whose first word's low bytes are a shared prefix put 20,000 groups in a few thousand buckets.
#[derive(Default)]
struct Words(u64);

impl Hasher for Words {
    fn finish(&self) -> u64 {
        self.0.rotate_left(26)
    }
    fn write(&mut self, b: &[u8]) {
        b.chunks(8).for_each(|c| self.write_u64(c.iter().rev().fold(0, |w, &x| w << 8 | x as u64)));
    }
    fn write_u64(&mut self, x: u64) {
        self.0 = (self.0.rotate_left(5) ^ x).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

impl Slot {
    pub(crate) fn charge(&self) {
        self.bytes.store(self.node.aggcontext().context().subtree_allocated() as u64, Ordering::Relaxed);
    }
}

impl AggArena {
    /// The arena over the Agg's aggregates (`aggno` each) and kept columns of types `kept` (aggno `naggs + i`, D345) at
    /// `width`; ERROR by name for a shape it does not host. EXECUTE checks + catalog once (bind). D3025: a kept column
    /// does not make it serial: each worker slot keeps its first row's value and the combine keeps the earlier partial's,
    /// an input row's member of the group (a key's members are equal under its equality, X15).
    pub fn new(aggrefs: &[&'static Aggref<'static>], kept: &[Oid], width: u16) -> PgResult<Box<AggArena>> {
        let keep = gravity_driver::mem::root::context("gravity agg arena init", true);
        let (specs, parallel) = crate::ops::agg::trans::hosted_bind::specs(aggrefs, width, &keep)?;
        // V4R-453: a parallel arena's workers read the catalog facts resolved here; None (a fact a worker cannot be armed
        // with: a row type, a domain, a non-builtin function) runs it serial
        let catalog = if parallel { crate::ops::agg::trans::hosted_catalog::catalog(&specs)?.map(Box::new) } else { None }; // alloc: Exec
        let parallel = parallel && catalog.is_some();
        let kept = kept.iter().map(|&t| gravity_fn::typesem::storage::len_byval(t)).collect::<PgResult<Vec<_>>>()?; // alloc: Exec
        let naggs = aggrefs.len() as u32;
        let n = if parallel { width as usize + 1 } else { 1 };
        let slots = (0..n).map(|_| Slot::new(&specs, parallel, &keep)).collect::<PgResult<Vec<_>>>()?; // alloc: Exec
        let head = AggHead { magic: AGG_MAGIC, trans: raw::trans_of, final_: raw::final_of, fold: raw::fold_of, combine: raw::combine_of, combine_keep: raw::combine_keep_of, home: raw::home_of, homes_clear: raw::homes_clear_of, direct: 0, given: 0, stable: 0, wide: 1, gather: raw::gather_of };
        Ok(Box::new(AggArena { head, specs, slots, parallel, keep, kept, naggs, homes: home_cells(), gathered: HashSet::new(), gathered_charge: Charge::NONE, owner: pgsync::OnceLock::new(), catalog, finals: Vec::new(), streamed: false, handed: false, budget: (crate::inputs::work_mem_kb().max(64) as u64) << 10 })) // alloc: Exec
    }

    /// The arena of an Agg whose keyed record holds its states. D3062-b: wide at `width` when parallel (PG's rule) and no
    /// aggregate is an ordered collect (`types_core::quiet::ORDERED_COMBINES`): each home holds a state per worker slot and the gathered one
    /// (`AggHead.wide`), so the folds keep their width; else the V4R-375 arena, serial at width 1.
    pub fn new_keyed(aggrefs: &[&'static Aggref<'static>], kept: &[Oid], width: u16) -> PgResult<Box<AggArena>> {
        if width > 1 {
            let mut a = AggArena::new(aggrefs, kept, width)?;
            if a.parallel && !a.specs.iter().any(|s| types_core::quiet::ordered_combine(s.shape.aggcombinefn)) {
                a.head.wide = a.slots.len() as u64;
                return Ok(a);
            }
        }
        AggArena::new(aggrefs, kept, 1)
    }

    /// D3062-b: whether each keyed home holds a state per worker (`new_keyed`).
    pub fn wide(&self) -> bool {
        self.head.wide > 1
    }

    /// The handle word the program reads (`AGG_W_HEAD`): the arena's address.
    pub fn handle(&mut self) -> u64 {
        &mut self.head as *mut AggHead as u64
    }

    /// D2332: the arena's Agg is plain (no groups, no keyed record): each final's by-reference image is kept in the
    /// aggregate's own buffer until its next final (`AggHead.stable`), not copied into the Call's scratch per re-open.
    pub fn plain_fed(&mut self) {
        self.head.stable = 1;
    }

    /// The arena's Agg is a streamed sorted group (lowerv4 group/stream.rs): its finals run when the group is handed on,
    /// before the next group's first row is folded into a fresh record. nodeAgg.c agg_retrieve_direct resets each
    /// aggcontext (ReScanExprContext: the shutdown callbacks, then the memory) when it starts the next group, so a group's
    /// states never outlive it: an ordered-set aggregate's sort (ordered_set_shutdown's tuplesort_end), a collect's buffer.
    /// D3578: each final's by-reference image is kept in the aggregate's own buffer until its next final (`AggHead.stable`,
    /// D2332's road), as PostgreSQL's per-output-tuple context is reset per group: the hand-on's steps copy what they keep
    /// before the next group's final runs, while a copy into the Call's scratch stayed to the call's end, and a sorted
    /// input's emit feeding a fold above runs as one call (100,000 groups of 2 kB finals held 200 MB to the end).
    pub fn streamed(&mut self) {
        self.streamed = true;
        self.head.stable = 1;
    }

    /// The first transition after a streamed group's finals: the handed-on group's states go (`streamed`). Each final
    /// copied its image out of the arena (kernels `m6_agg_final`), so nothing the emitted row reads lives here.
    fn next_group(&mut self, k: usize) {
        self.handed = false;
        let s = &mut self.slots[k];
        s.node.reset();
        s.spill = Default::default();
        s.tmp.reset();
        s.calls = 0;
        s.bytes.store(0, Ordering::Relaxed);
    }

    /// Whether the program keeps its widths: one slot per worker ordinal and the partial states combined.
    pub fn parallel(&self) -> bool {
        self.parallel
    }

    /// D2289: a re-open of the rerun set the Agg runs in (PG's ExecReScanAgg resetting its aggcontexts): each slot's
    /// shutdown callbacks run and its aggcontext is reset (kept), and the keyed homes go. The set's breaker slots are
    /// zeroed by the re-open too, so no record still points at a state freed here; a final's result was copied out of the
    /// arena (kernels `m6_agg_final`). Without it every re-open's by-reference states stayed to the statement's end.
    pub fn reset(&mut self) {
        // bounded("small-fixed: one slot per worker ordinal and the merge slot (`AggArena::new`: width + 1), at most driver section MAX_WIDTH + 1 (256)")
        for s in self.slots.iter_mut() {
            s.node.reset();
            s.spill = Default::default();
            s.tmp.reset();
            s.calls = 0;
            s.bytes.store(0, Ordering::Relaxed);
        }
        self.homes.iter_mut().filter_map(|c| c.get_mut()).for_each(|m| *m.get_mut().unwrap_or_else(|e| e.into_inner()) = Homes::default());
        self.gathered.clear();
        // the next open's homes take its own slot's owner (a re-open may begin another)
        self.owner = pgsync::OnceLock::new();
        self.handed = false;
    }

    /// Every slot's aggcontext bytes and the keyed homes' own (the grant's charge). CONVERT (wave 2, H1): each shard's
    /// waiting heap bytes are committed to the homes' owner here (the caller is the leader between pipelines).
    pub fn bytes(&self) -> u64 {
        let owner = self.homes_owner();
        let homes = self.shards().map(|m| {
            let mut hm = m.lock().unwrap_or_else(|e| e.into_inner());
            hm.commit(owner);
            hm.bytes
        });
        let homes = homes.sum::<u64>();
        self.slots.iter().map(|s| s.bytes.load(Ordering::Relaxed)).sum::<u64>() + homes + 16 * self.gathered.len() as u64
    }

    /// CONVERT (wave 2, H1): the bytes a home shard's charge commits at once (one commit, not one per group: a commit is
    /// atomics on the owner's cell, which every folding thread shares). The 64 shards together then lag the ledger by at
    /// most what the arena's threads (its slots: the workers and the leader's merge slot, or the one serial slot) may lag
    /// it themselves, `memledger::DOOR_FOLD_BYTES` each before a thread's account folds.
    fn homes_batch(&self) -> u64 {
        memledger::DOOR_FOLD_BYTES as u64 * self.slots.len() as u64 / HOME_SHARDS as u64
    }

    /// pglserve (D4601): home shard `i`, made at its first home.
    fn shard(&self, i: usize) -> &Mutex<Homes> {
        self.homes[i].get_or_init(Mutex::default)
    }

    /// pglserve (D4601): the home shards made so far (an unmade one holds no home, no bytes and no charge).
    fn shards(&self) -> impl Iterator<Item = &Mutex<Homes>> {
        self.homes.iter().filter_map(|c| c.get())
    }

    /// CONVERT (wave 2, H1): the homes' owner once a home read it (`raw::home_of`), else None.
    fn homes_owner(&self) -> Option<(Owner, Kind)> {
        self.owner.get().copied().flatten()
    }

    /// CONVERT (wave 2, H1): `gathered`'s table re-read after an insert: its growth committed to the homes' owner (a few
    /// commits: the table doubles). A re-open keeps the table (`reset` clears it) and its charge.
    fn gathered_charged(&mut self) {
        let (t, had) = (table_bytes(self.gathered.capacity(), core::mem::size_of::<usize>()), self.gathered_charge.committed());
        if let Some(o) = self.homes_owner().filter(|_| t > had) {
            open_on(&mut self.gathered_charge, o);
            self.gathered_charge.commit(t - had);
        }
    }

    pub(crate) fn agg(&self, aggno: u32) -> PgResult<usize> {
        self.specs.iter().position(|s| s.a.aggno == aggno as i32).ok_or_else(|| Box::new(PgError::error(format!("gravity: no hosted aggregate {aggno}")))) // alloc: Cold
    }

    /// Worker `ordinal`'s slot: a serial arena has the one (the program runs width 1), a parallel one a slot per ordinal
    /// before the merge slot. D1192: a run nested inside an outer row (`Entry::out_of_order`: a lazy build's fed producers,
    /// an InitPlan's members) is the leader's, on a nested hosted lane's ordinal (D1036): it folds in the leader's slot
    /// (ordinal 0). An arena is one Agg's, never re-entered by an outer row while that run folds it.
    fn slot(&self, ordinal: u16) -> PgResult<usize> {
        let n = if self.parallel { self.slots.len() - 1 } else { 1 };
        let o = if ordinal >= gravity_driver::NESTED_LANE_FIRST { 0 } else { ordinal as usize };
        (o < n).then_some(o).ok_or_else(|| Box::new(PgError::error(format!("gravity: a hosted aggregate called by worker {ordinal} of an arena of {n}")))) // alloc: Cold
    }

    /// V4R-452 (D1409a): before a slot's first transition, on a pool claim, every aggregate's collation is armed (bind
    /// resolved it on the leader); else a named ERROR, not a catcache panic. Once per slot, not per row.
    fn collations_armed(&self, k: usize) -> PgResult<()> {
        self.arm_catalog();
        if self.slots[k].calls != 0 {
            return Ok(());
        }
        self.specs.iter().try_for_each(|s| gravity_fn::typesem::collation::check_worker_collation(s.a.inputcollid))
    }

    /// V4R-453 (D1409): on a pool claim, this thread armed with the arena's catalog facts (m6_agg_catalog.rs).
    #[inline]
    fn arm_catalog(&self) {
        if let Some(c) = self.catalog.as_deref() {
            c.arm();
        }
    }

}

/// D2337 / D2336: a final's result word as the emitted code reads it, whole: an int2 / int4 result sign-extended. A
/// function-manager body returns PG's zero-extended `Int32GetDatum` word (C reads it through `DatumGetInt32`), so a user
/// final (or, with no finalfn, transition) function returning int4 / int2, or a domain over one, compared and summed as
/// 2^32 + h. Any other word as it is.
fn result_word(word: Oid, v: u64) -> u64 {
    match word {
        INT2OID => v as i16 as i64 as u64,
        INT4OID => v as i32 as i64 as u64,
        _ => v,
    }
}

const INT2OID: Oid = 21;
const INT4OID: Oid = 23;

impl Drop for AggArena {
    fn drop(&mut self) {
        self.head.magic = 0;
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use core::hash::BuildHasher;

    /// 20,000 tagged text images `'key-' || 9 digits` (key.tagged: bytes 0..7, bytes 8..12 | 13 << 56) spread over a
    /// 2^15-bucket map's low bits as random hashes would (about 14,900 buckets used); the multiply-only step used ~3,200.
    #[test]
    fn home_hash_spreads_tagged_text_images() {
        let b = BuildHasherDefault::<Words>::default();
        let mut used = std::collections::HashSet::new();
        for i in 0..20_000u32 {
            let s = format!("key-{i:09}"); // alloc: Cold
            let mut w = [0u8; 16];
            w[..s.len()].copy_from_slice(s.as_bytes());
            let img: Box<[u64]> = Box::new([u64::from_le_bytes(w[..8].try_into().unwrap()), u64::from_le_bytes(w[8..].try_into().unwrap()) | (s.len() as u64) << 56]); // alloc: Cold
            used.insert(b.hash_one(&img) & ((1 << 15) - 1));
        }
        assert!(used.len() > 13_000, "{} buckets", used.len());
    }
}
