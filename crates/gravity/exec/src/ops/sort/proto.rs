//! M3's runtime protocols (M3-plan.md §6, cards/M3.md L3, Decision 7). Grouping sets take M2's `append.arms` (one arm
//! pipeline and one owner phase per set, decompose's union rewrite; no protocol of their own). MergeAppend
//! (`ArmSort`): an append.arms run whose slot is a `runs.sort` one; each continuing arm's sealed run moves
//! into a statement-lived region (`try_region_in`, charged to the grant pool), the last arm's after-pipeline merges them
//! all with its own into the slot's one sealed run (one k-way, k the arms), and once the slot's stream holds a run (an arm
//! spilled) every held run is put to it, the rest of the run the stream's k-way. The sort protocol is m3_sort.rs; the
//! walk's `claims_for` m3_walk.rs. Created by M3 (owner M3-HOSTS).
use core::ops::Range;
use core::sync::atomic::Ordering::AcqRel;

use gravity_abi::breaker::{SortRun, SORTRUN_HDR, SORTRUN_VAR_WORDS};
use gravity_driver::{BorrowedBatch, Error, Exec, Flow, Merge, RegionBuf, Result, Sink, SinkRef, State};
use gravity_fn::arena::Owner;
use gravity_kernels::ops::sort::entries::RunShape;
use gravity_program::Program;

use super::raw;
use crate::ops::{Live, Protocol};
use crate::admit::{Admit, Bind};


pub const ADMIT: &[Admit] = &[crate::ops::sort::fetch::admit];
pub const BIND: &[Bind] = &[crate::ops::sort::fetch::bind];
pub const PROTOCOLS: &[&dyn Protocol] = &[&crate::ops::sort::merge::SortMerge, &crate::ops::sort::walk::Walk, &crate::ops::sort::cosplit::CoSplit, &crate::ops::sort::heads::Heads];

/// MergeAppend's arms over one `runs.sort` slot: each arm merged as a sort's section (`Resident`), its run held after it.
pub(crate) struct ArmSort {
    run: crate::ops::sort::merge::Resident,
    slot: u8,
    /// The run's pipelines that produce `slot` (the arms); a feeder between two arms (M3-MERGE) is not one.
    prods: memledger::ChargedVec<usize>,
    held: pgsync::Mutex<Vec<RegionBuf>>,
}

/// A sink no row reaches (a merge's layout).
struct NoRows;

impl Sink for NoRows {
    fn push(&mut self, _: BorrowedBatch<'_>) -> Result<Flow> {
        Err(Error::host("gravity: runs.sort: a row pushed during a merge's layout"))
    }
}

impl ArmSort {
    pub(crate) fn new(run: crate::ops::sort::merge::Resident, slot: u8, program: &Program, mut prods: memledger::ChargedVec<usize>) -> Box<dyn Live> {
        prods.with_vec(|v| v.extend(run.arms.clone().filter(|&q| program.pipelines.get(q).and_then(|x| x.produces()) == Some(slot)))); // alloc: Exec charged prods.owner()
        Box::new(ArmSort { run, slot, prods, held: pgsync::Mutex::new(Vec::new()) }) // alloc: Exec
    }

    fn is_arm(&self, p: usize) -> bool {
        self.prods.contains(&p)
    }

    /// After arm `p`: its run held (a continuing arm, or the last with others held), put once the stream holds a run,
    /// else at the last arm every held run merged into the leader. A hold or the leader's merge past the grant (the
    /// region refused) puts instead: every held run and the arm's own sealed runs go to the stream, as an arm that spills.
    fn after(&self, p: usize, node: &mut dyn ArmNode) -> Result<()> {
        let (last, slot, var) = (self.prods.last() == Some(&p), self.slot, self.run.fixed.is_none());
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        // D3918: the run ended at its first arm `p` (driver `Node::arm_ends_run`: every later arm is a spilled build's pair
        // pass over a build that wrote no partition). `p` finished and sealed the slot's whole run in the leader as a single
        // producer does, so it is neither held nor re-laid, and the later arms are stepped past unopened (`arm_ended`).
        if !last && node.ends_run(p) {
            debug_assert!(held.is_empty(), "a run that ends at its first arm holds no earlier arm's run");
            fires::fire("sort.arms.alone"); // shapecheck fire signal (D3918): a pair arm with no row left the run in place
            // past the cap `finish` left the workers' sealed runs in `run.held` (no `over` under an ArmSort): they are put
            // to the slot's stream, as a single producer's `after_pipeline` puts them (`Resident::put_over`)
            let over = core::mem::take(&mut *self.run.held.lock().unwrap_or_else(|e| e.into_inner()));
            return if over.is_empty() { Ok(()) } else { self.run.put_over(node, slot, over) };
        }
        if !(last && held.is_empty()) {
            let leader = node.leader(slot);
            let (mut runs, rb, shape) = match settle(&self.run, leader) {
                Some(srcs) => self.run.runs(srcs.into_iter())?,
                None => self.run.runs(core::iter::once((leader.as_ptr() as usize, leader.len())))?,
            };
            if let Some(s) = shape {
                match merged(&mut |n| node.region(n), &mut runs, rb, s, var, self.run.cap) {
                    Ok(b) => held.push(b), // alloc: Exec
                    Err(Error::OutOfMemory { .. }) => {
                        put_held(node, slot, &mut held, var)?;
                        let owner = node.slot_owner(slot);
                        for r in runs.iter() {
                            if var {
                                raw::compact(&s, r.0, owner)?;
                            }
                            node.put_run(slot, head(r.0) as *mut u8)?;
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            rearm(node.leader(slot));
        }
        if node.sort_runs(slot) > 0 {
            return put_held(node, slot, &mut held, var);
        }
        if !last || held.is_empty() {
            return Ok(());
        }
        let (mut runs, rb, shape) = self.run.runs(held.iter().map(|b| (b.as_ptr() as usize, b.len())))?;
        let s = shape.expect("a held run");
        let mut sink = NoRows;
        let mut leader = node.leader_state(slot, SinkRef::of(&mut sink));
        let vb = runs.iter().map(|r| raw::var_bytes(&s, r.0)).sum();
        let (rows, var0) = match crate::ops::sort::merge::Resident::lay(&mut leader, runs.iter().map(|r| r.1 as u64).sum(), rb, vb, var.then_some(runs[0].0), self.run.cap) {
            Ok(at) => at,
            Err(Error::OutOfMemory { .. }) => {
                drop(leader);
                rearm(node.leader(slot));
                return put_held(node, slot, &mut held, var);
            }
            Err(e) => return Err(e),
        };
        if !raw::merge_laid(&s, &mut runs, rows, leader.bytes.as_mut_ptr(), var0) {
            return Err(Error::host("gravity: runs.sort: a merge compare failed"));
        }
        raw::seal(&mut leader.bytes[..]);
        held.clear();
        Ok(())
    }
}

/// What `ArmSort` (and `Resident::put_over`) asks of the execution (the Node's; a test's stand-in).
pub(crate) trait ArmNode {
    fn region(&mut self, bytes: usize) -> Result<RegionBuf>;
    fn leader(&mut self, slot: u8) -> &mut [u8];
    fn leader_state<'a>(&'a mut self, slot: u8, sink: SinkRef<'a>) -> State<'a>;
    fn sort_runs(&self, slot: u8) -> u32;
    /// `run` put to the slot's stream: a sealed run in its put form that nothing else reads (`Node::put_sort_run`).
    fn put_run(&mut self, slot: u8, run: *mut u8) -> Result<u32>;
    /// D3918: whether the run ends at arm `p`, its first (`Node::arm_ends_run`: each later arm an empty pair pass).
    fn ends_run(&self, _p: usize) -> bool {
        false
    }
    /// CONVERT wave 2: the slot's ledger owner (`Node::slot_owner`), whose row a put's compact copy is on.
    fn slot_owner(&self, _slot: u8) -> Option<Owner> {
        None
    }
}

impl ArmNode for Exec {
    fn region(&mut self, bytes: usize) -> Result<RegionBuf> {
        gravity_driver::try_region_in(&self.lease(), bytes)
    }
    fn leader(&mut self, slot: u8) -> &mut [u8] {
        self.node_mut().slot_region(slot)
    }
    fn leader_state<'a>(&'a mut self, slot: u8, sink: SinkRef<'a>) -> State<'a> {
        self.node_mut().leader_state(slot, sink)
    }
    fn sort_runs(&self, slot: u8) -> u32 {
        self.node().sort_runs(slot)
    }
    fn put_run(&mut self, slot: u8, run: *mut u8) -> Result<u32> {
        raw::put_sort_run(self, slot, run)
    }
    fn ends_run(&self, p: usize) -> bool {
        self.node().arm_ends_run(p)
    }
    fn slot_owner(&self, slot: u8) -> Option<Owner> {
        self.node().slot_owner(slot)
    }
}

impl Live for ArmSort {
    fn answers(&self) -> u16 {
        crate::ops::ask::MERGE | crate::ops::ask::AFTER
    }

    fn merge_for(&self, pipeline: usize) -> Option<&dyn Merge> {
        self.is_arm(pipeline).then_some(&self.run as &dyn Merge)
    }
    fn after_pipeline(&self, pipeline: usize, exec: &mut Exec) -> Option<Result<()>> {
        self.is_arm(pipeline).then(|| self.after(pipeline, exec))
    }
}

/// The leader's region armed afresh as a share (`at_share` lays the next serial arm's run over all of it).
pub(crate) fn rearm(leader: &mut [u8]) {
    if leader.len() >= SORTRUN_HDR {
        raw::arm_share(leader);
    }
}

/// The arm's run sources: the partials `merge_into` noted (`Some`), else `None`: the leader's run (phase 2's laid run sealed)
/// is the one source.
fn settle(run: &crate::ops::sort::merge::Resident, leader: &mut [u8]) -> Option<Vec<(usize, usize)>> {
    let held = core::mem::take(&mut *run.held.lock().unwrap_or_else(|e| e.into_inner()));
    let cut = core::mem::take(&mut *run.cut.lock().unwrap_or_else(|e| e.into_inner()));
    if !held.is_empty() {
        return Some(held);
    }
    if !cut.runs.is_empty() && run.done.swap(0, AcqRel) + 1 == cut.out.len() {
        // phase 2 laid the run header at the leader's base
        raw::seal(leader);
    }
    None
}

/// A sealed run's header from its rows' base.
pub(crate) fn head(base: usize) -> *const SortRun {
    (base - SORTRUN_HDR) as *const SortRun
}

/// `runs` merged into a statement-lived run of `lay`'s layout (a worker's: it reads back through `RunShape::of_run`
/// and compacts for a put in place).
/// D3259: past `max` bytes none is laid (`OutOfMemory`: the caller puts the runs), before any region is drawn.
fn merged(region: &mut dyn FnMut(usize) -> Result<RegionBuf>, runs: &mut [(usize, u32)], rb: u32, s: RunShape, var: bool, max: u64) -> Result<RegionBuf> {
    let vb: u64 = runs.iter().map(|r| raw::var_bytes(&s, r.0)).sum();
    let total: u64 = runs.iter().map(|r| r.1 as u64).sum();
    let copy = if var { raw::copy_bytes(runs[0].0) as u64 } else { 0 };
    let need = SORTRUN_HDR as u64 + ((total * rb as u64 + vb + copy).div_ceil(rb as u64) + 1) * rb as u64 + SORTRUN_VAR_WORDS as u64;
    if need > max {
        return Err(Error::OutOfMemory { bytes: need, held: 0, capacity: max, why: None });
    }
    let mut buf = region(SORTRUN_HDR + ((total * rb as u64 + vb + copy).div_ceil(rb as u64) as usize + 1) * rb as usize + SORTRUN_VAR_WORDS as usize)?;
    let mut sink = NoRows;
    let mut st = State::new(&mut buf[..], SinkRef::of(&mut sink));
    let (rows, var0) = crate::ops::sort::merge::Resident::lay(&mut st, total, rb, vb, var.then_some(runs[0].0), max)?;
    if !raw::merge_laid(&s, runs, rows, st.bytes.as_mut_ptr(), var0) {
        return Err(Error::host("gravity: runs.sort: a merge compare failed"));
    }
    raw::seal(&mut st.bytes[..]);
    Ok(buf)
}

/// Every held run put to `slot`'s stream in its put form (the reader then merges the stream).
fn put_held(node: &mut dyn ArmNode, slot: u8, held: &mut Vec<RegionBuf>, var: bool) -> Result<()> {
    let owner = node.slot_owner(slot);
    for mut b in held.drain(..) {
        node.put_run(slot, raw::put_form(&mut b, var, owner)?)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use crate::ops::sort::merge::tests::{check, partials, run, vcheck, vrun, Discard, RB};
    use crate::ops::sort::merge::Resident;
    use super::*;
    use gravity_kernels::ops::sort::entries::{RESIDENT_MAX, RUN_SEALED};
    use gravity_driver::{PartitionPlan, Region};

    fn cv(v: impl IntoIterator<Item = usize>) -> memledger::ChargedVec<usize> {
        let mut c = memledger::ChargedVec::none();
        c.with_vec(|x| x.extend(v)); // alloc: Cold
        c
    }

    /// The Node's part `ArmSort` reads: the leader's slot region and the slot's stream (a put run's dense bytes).
    struct Mock {
        leader: Vec<u64>,
        stream: Vec<Vec<u8>>,
        /// A hold past the grant: `region` refuses as the pool does.
        oom: bool,
        /// D3918: the arms a run ends at (`ends_run`: each later arm a pair pass over a build that wrote no partition).
        ends: Vec<usize>,
    }

    fn bytes(v: &mut [u64]) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, v.len() * 8) }
    }

    impl ArmNode for Mock {
        fn region(&mut self, n: usize) -> Result<RegionBuf> {
            if self.oom {
                return Err(Error::OutOfMemory { bytes: n as u64, held: 0, capacity: 0, why: None });
            }
            gravity_driver::try_region(n)
        }
        fn leader(&mut self, _: u8) -> &mut [u8] {
            bytes(&mut self.leader)
        }
        fn leader_state<'a>(&'a mut self, _: u8, sink: SinkRef<'a>) -> State<'a> {
            State::new(bytes(&mut self.leader), sink)
        }
        fn sort_runs(&self, _: u8) -> u32 {
            self.stream.len() as u32
        }
        fn put_run(&mut self, _: u8, run: *mut u8) -> Result<u32> {
            let r = run as *mut SortRun;
            // SAFETY: as `Node::put_sort_run`: a sealed run in its put form that nothing else reads.
            unsafe {
                self.stream.push(core::slice::from_raw_parts((*r).rows_ptr() as *const u8, (*r).dense_bytes()).to_vec()); // alloc: Cold
                (*r).clear();
            }
            Ok(self.stream.len() as u32 - 1)
        }
        fn ends_run(&self, p: usize) -> bool {
            self.ends.contains(&p)
        }
    }

    fn arms(n: usize, fixed: Option<(usize, u64)>) -> (ArmSort, Mock) {
        let run = Resident::new(0..n, fixed, 0, None);
        (ArmSort { run, slot: 0, prods: cv(0..n), held: pgsync::Mutex::new(Vec::new()) }, Mock { leader: vec![0; 1 << 15], stream: Vec::new(), oom: false, ends: Vec::new() }) // alloc: Cold
    }

    /// Arm `p`'s partials through the leader's own fold, then its finish when it is the last arm.
    fn fold(a: &ArmSort, m: &mut Mock, p: usize, runs: &[Vec<u64>]) {
        for part in partials(runs) {
            a.merge_for(p).unwrap().merge_into(&mut State::new(&mut [], SinkRef::of(&mut Discard)), part).unwrap();
        }
        if a.prods.last() == Some(&p) {
            a.merge_for(p).unwrap().finish(&mut State::new(bytes(&mut m.leader), SinkRef::of(&mut Discard))).unwrap();
        }
    }

    /// Three arms, each a path of its own: phase 2 into the leader (continuing), a serial arm's run sealed in the leader,
    /// the last arm's partials folded on the leader; the arms' runs held and merged once into the leader's sealed run.
    #[test]
    fn a_merge_append_holds_each_arms_run_and_merges_them_once() {
        let (a, mut m) = arms(3, Some((2, 0)));
        let (a0, a1) = (run(&[9, 1, 5, 7], 1), run(&[2, 8, 8], 2));
        let both = [a0.clone(), a1.clone()];
        let ps = partials(&both);
        let plan = PartitionPlan { pbits: 1, groups: 7, record: RB as u32 };
        let merge = a.merge_for(0).unwrap();
        merge.prepare_partitions_in(&mut State::new(bytes(&mut m.leader), SinkRef::of(&mut Discard)), plan, &ps).unwrap();
        for part in [1, 0] {
            merge.merge_partition(Region { ptr: m.leader.as_mut_ptr() as *mut u8, len: m.leader.len() * 8 }, part, plan, &ps).unwrap();
        }
        a.after(0, &mut m).unwrap();
        let b0 = run(&[3, 0, 6], 3);
        m.leader[..b0.len()].copy_from_slice(&b0);
        a.after(1, &mut m).unwrap();
        let (c0, c1) = (run(&[4, 4, 10], 4), run(&[1], 5));
        fold(&a, &mut m, 2, &[c0.clone(), c1.clone()]);
        a.after(2, &mut m).unwrap();
        check(&m.leader, &[a0, a1, b0, c0, c1]);
        assert!(m.stream.is_empty() && a.held.lock().unwrap().is_empty(), "nothing put, nothing held");
    }

    /// D3918: the run ends at arm 0 (arm 1 a pair pass with no row): arm 0's phase-2 run, finished and sealed in the leader
    /// as a single producer's, is neither held nor re-laid, and the stepped-past arm 1's `after` leaves it there.
    #[test]
    fn a_run_that_ends_at_its_first_arm_keeps_its_run_in_place() {
        let (a, mut m) = arms(2, Some((2, 0)));
        m.ends = vec![0]; // alloc: Cold
        let (a0, a1) = (run(&[9, 1, 5, 7], 1), run(&[2, 8, 8], 2));
        let both = [a0.clone(), a1.clone()];
        let ps = partials(&both);
        let plan = PartitionPlan { pbits: 1, groups: 7, record: RB as u32 };
        let merge = a.merge_for(0).unwrap();
        merge.prepare_partitions_in(&mut State::new(bytes(&mut m.leader), SinkRef::of(&mut Discard)), plan, &ps).unwrap();
        for part in [1, 0] {
            merge.merge_partition(Region { ptr: m.leader.as_mut_ptr() as *mut u8, len: m.leader.len() * 8 }, part, plan, &ps).unwrap();
        }
        merge.finish(&mut State::new(bytes(&mut m.leader), SinkRef::of(&mut Discard))).unwrap();
        let before = m.leader.clone();
        a.after(0, &mut m).unwrap();
        assert!(a.held.lock().unwrap().is_empty() && m.leader == before, "arm 0's run neither held nor touched");
        a.after(1, &mut m).unwrap();
        assert!(m.leader == before, "the stepped-past arm's after leaves the sealed run");
        check(&m.leader, &[a0, a1]);
        assert!(m.stream.is_empty() && a.held.lock().unwrap().is_empty(), "nothing put, nothing held");
    }

    /// Arm 1 spills (its runs went to the stream): arm 0's held run is put behind them, every row in the stream once.
    #[test]
    fn an_arm_that_spills_puts_every_held_run_and_the_emit_sees_arm_0() {
        let (a, mut m) = arms(2, Some((2, 0)));
        let arm0 = [run(&[9, 1, 5], 1), run(&[2, 8], 2)];
        fold(&a, &mut m, 0, &arm0);
        a.after(0, &mut m).unwrap();
        let d = run(&[7, 3], 3);
        m.stream.push(bytes(&mut d.clone())[SORTRUN_HDR..][..2 * RB].to_vec()); // alloc: Cold
        fold(&a, &mut m, 1, &[]);
        a.after(1, &mut m).unwrap();
        assert_eq!(m.stream.len(), 2, "arm 1's run and arm 0's held run");
        let rows = |b: &[u8]| b.chunks(RB).map(|r| (u64::from_ne_bytes(r[8..16].try_into().unwrap()), u64::from_ne_bytes(r[16..24].try_into().unwrap()))).collect::<Vec<_>>();
        let held = rows(&m.stream[1]);
        assert!(held.windows(2).all(|w| w[0].0 <= w[1].0), "the held run in key order");
        let mut got: Vec<u64> = m.stream.iter().flat_map(|b| rows(b)).map(|r| r.0).collect();
        got.sort_unstable();
        assert_eq!(got, vec![1, 2, 3, 5, 7, 8, 9]); // alloc: Cold
        assert_ne!(unsafe { (*(m.leader.as_ptr() as *const SortRun)).pad[0] }, RUN_SEALED, "no sealed run beside the stream");
    }

    /// M3-MERGE's feeders: in the run 0..4 only 1 and 3 produce the slot; 0 and 2 are no arm (no merge, no after), the
    /// last producer seals the one merged run.
    #[test]
    fn a_feeder_between_the_arms_is_no_arm() {
        let (mut a, mut m) = arms(4, Some((2, 0)));
        a.prods = cv([1, 3]);
        assert!(a.merge_for(0).is_none() && a.merge_for(2).is_none() && a.merge_for(1).is_some());
        let (x, y) = ([run(&[5, 1], 1)], [run(&[4, 2], 2)]);
        fold(&a, &mut m, 1, &x);
        a.after(1, &mut m).unwrap();
        assert_eq!(a.held.lock().unwrap().len(), 1, "arm 1 continues: its run held");
        fold(&a, &mut m, 3, &y);
        a.after(3, &mut m).unwrap();
        check(&m.leader, &[x[0].clone(), y[0].clone()]);
    }

    /// MERGE's K=10 cell (out of memory past the grant): a hold the grant refuses puts instead, the held runs and the arm's
    /// own, and the arms after it put as a spilling arm's do; every row reaches the stream once, in key order per run.
    #[test]
    fn a_hold_past_the_grant_puts_every_run_instead() {
        let (a, mut m) = arms(3, Some((2, 0)));
        let (x, y) = ([run(&[9, 1, 5], 1)], [run(&[2, 8], 2)]);
        fold(&a, &mut m, 0, &x);
        a.after(0, &mut m).unwrap();
        assert_eq!((a.held.lock().unwrap().len(), m.stream.len()), (1, 0), "arm 0 held");
        m.oom = true;
        fold(&a, &mut m, 1, &y);
        a.after(1, &mut m).unwrap();
        assert_eq!((a.held.lock().unwrap().len(), m.stream.len()), (0, 2), "arm 0's held run and arm 1's put");
        let z = run(&[7, 3], 3);
        m.stream.push(bytes(&mut z.clone())[SORTRUN_HDR..][..2 * RB].to_vec()); // alloc: Cold
        fold(&a, &mut m, 2, &[]);
        a.after(2, &mut m).unwrap();
        let keys = |b: &[u8]| b.chunks(RB).map(|r| u64::from_ne_bytes(r[8..16].try_into().unwrap())).collect::<Vec<_>>();
        assert!(m.stream.iter().all(|b| keys(b).windows(2).all(|w| w[0] <= w[1])), "each put run in key order");
        let mut got: Vec<u64> = m.stream.iter().flat_map(|b| keys(b)).collect();
        got.sort_unstable();
        assert_eq!(got, vec![1, 2, 3, 5, 7, 8, 9]); // alloc: Cold
        assert_ne!(unsafe { (*(m.leader.as_ptr() as *const SortRun)).pad[0] }, RUN_SEALED, "no sealed run beside the stream");
    }

    /// D3259: kernels `resident` seals whenever no worker put, so an `ArmSort` merge can pass the cap (a region's bytes)
    /// too: arm 0's hold fits it, the last arm's merge of both does not, and every run is put instead (no error, no sealed
    /// run beside the stream), each in key order, every row once.
    #[test]
    fn arms_whose_merge_passes_the_cap_put_their_runs_instead() {
        let (mut a, mut m) = arms(2, Some((2, 0)));
        a.run.cap = crate::ops::sort::merge::laid_bytes(3, RB as u32, 0);
        let (x, y) = ([run(&[9, 1, 5], 1)], [run(&[2, 8], 2)]);
        fold(&a, &mut m, 0, &x);
        a.after(0, &mut m).unwrap();
        assert_eq!((a.held.lock().unwrap().len(), m.stream.len()), (1, 0), "arm 0's 3 rows fit the cap: held");
        fold(&a, &mut m, 1, &y);
        a.after(1, &mut m).unwrap();
        assert_eq!((a.held.lock().unwrap().len(), m.stream.len()), (0, 2), "5 rows pass it: both runs put");
        let keys = |b: &[u8]| b.chunks(RB).map(|r| u64::from_ne_bytes(r[8..16].try_into().unwrap())).collect::<Vec<_>>();
        assert!(m.stream.iter().all(|b| keys(b).windows(2).all(|w| w[0] <= w[1])), "each put run in key order");
        let mut got: Vec<u64> = m.stream.iter().flat_map(|b| keys(b)).collect();
        got.sort_unstable();
        assert_eq!(got, vec![1, 2, 5, 8, 9]); // alloc: Cold
        assert_ne!(unsafe { (*(m.leader.as_ptr() as *const SortRun)).pad[0] }, RUN_SEALED, "no sealed run beside the stream");
    }

    /// Var rows: the held run keeps its table copy and copies (merged once into the leader), and in the put form its
    /// records carry them (a spill after it).
    #[test]
    fn var_arms_hold_their_copies_and_put_them_compacted() {
        let t = |w: &[&str]| w.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<Vec<u8>>>();
        let (x, y, z) = (t(&["pear", "apple and more", ""]), t(&["fig", "zucchini squash"]), t(&["kiwi", "apple"]));
        // the partials live until the arm's after-pipeline, as a worker's region lives until the next section's open
        let arm0 = [vrun(&x, 1), vrun(&y, 2)];
        let (a, mut m) = arms(2, None);
        fold(&a, &mut m, 0, &arm0);
        a.after(0, &mut m).unwrap();
        fold(&a, &mut m, 1, &[vrun(&z, 3)]);
        a.after(1, &mut m).unwrap();
        vcheck(&m.leader, &[x.clone(), y.clone(), z]);
        let (a, mut m) = arms(2, None);
        fold(&a, &mut m, 0, &arm0);
        a.after(0, &mut m).unwrap();
        m.stream.push(Vec::new()); // alloc: Cold
        fold(&a, &mut m, 1, &[]);
        a.after(1, &mut m).unwrap();
        let (mut got, b, mut at) = (Vec::new(), &m.stream[1], 0); // alloc: Cold
        while at < b.len() {
            let rec = u32::from_ne_bytes(b[at + 32..at + 36].try_into().unwrap()) as usize;
            let (size, off) = (u32::from_ne_bytes(b[at + 16..at + 20].try_into().unwrap()) as usize, u32::from_ne_bytes(b[at + 20..at + 24].try_into().unwrap()) as usize);
            got.push(b[at + 16 + off + 4..][..size - 4].to_vec());
            at += rec;
        }
        let mut want = [x, y].concat();
        want.sort();
        assert_eq!(got, want, "the records in text order, each with its copy");
    }

    /// D3918: a run that ends at its first arm, whose workers' sealed var runs pass the cap: `finish` lays none and leaves
    /// them in `run.held` (no `over` under an ArmSort), and the arm's `after` puts each to the stream, as a single
    /// producer's `after_pipeline` does; the stepped-past arm's `after` adds none.
    #[test]
    fn a_run_that_ends_at_its_first_arm_puts_its_runs_past_the_cap() {
        let t = |w: &[&str]| w.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<Vec<u8>>>();
        let (x, y) = (t(&["pear", "apple and more", ""]), t(&["fig", "zucchini squash"]));
        let runs = [vrun(&x, 1), vrun(&y, 2)];
        let ps = partials(&runs);
        let a = ArmSort { run: Resident { cap: 64, ..Resident::new(0..2, None, 0, None) }, slot: 0, prods: cv([0, 1]), held: pgsync::Mutex::new(Vec::new()) }; // alloc: Cold
        let mut m = Mock { leader: vec![0; 1 << 15], stream: Vec::new(), oom: false, ends: vec![0] }; // alloc: Cold
        for p in ps.iter() {
            a.run.merge_into(&mut State::new(&mut [], SinkRef::of(&mut Discard)), *p).unwrap();
        }
        a.run.finish(&mut State::new(bytes(&mut m.leader), SinkRef::of(&mut Discard))).unwrap();
        assert_eq!(a.run.held.lock().unwrap().len(), 2, "past the cap both sealed runs stay for the arm's after");
        a.after(0, &mut m).unwrap();
        assert_eq!(m.stream.len(), 2, "each sealed run put");
        assert!(a.run.held.lock().unwrap().is_empty() && a.held.lock().unwrap().is_empty(), "nothing left held");
        a.after(1, &mut m).unwrap();
        assert_eq!(m.stream.len(), 2, "the stepped-past arm puts none");
        assert_ne!(unsafe { (*(m.leader.as_ptr() as *const SortRun)).pad[0] }, RUN_SEALED, "no sealed run beside the stream");
    }

    /// D4413 (born red): the put of the sealed runs past the cap (`Resident::put_over`) compacts each to its put form on
    /// the leader between pipelines, where no kernel call lends it a byte: with the backend's abort bit set it stops in a
    /// run's compact copy and raises, nothing put and the run as it lay, so the same put with no abort puts every record.
    #[test]
    fn a_pending_abort_stops_the_put_past_the_cap_in_a_runs_compact() {
        let x: Vec<Vec<u8>> = (0..3 * gravity_abi::ctx::POLL_BATCH).map(|i| format!("text {:06}", (i * 7919) % 100_000).into_bytes()).collect(); // alloc: Cold
        let runs = [vrun(&x, 1)];
        let ps = partials(&runs);
        let m = Resident { cap: 64, ..Resident::new(0..1, None, 0, Some(0)) };
        let mut n = Mock { leader: vec![0; 1 << 15], stream: Vec::new(), oom: false, ends: Vec::new() }; // alloc: Cold
        for p in ps.iter() {
            m.merge_into(&mut State::new(&mut [], SinkRef::of(&mut Discard)), *p).unwrap();
        }
        m.finish(&mut State::new(bytes(&mut n.leader), SinkRef::of(&mut Discard))).unwrap();
        let spilled = core::mem::take(&mut *m.spilled.lock().unwrap());
        assert_eq!(spilled.len(), 1, "the sealed run left for the put");
        let abort = crate::glue::abort_u8();
        abort.fetch_or(gravity_abi::ctx::ABORT_BIT, AcqRel);
        let stopped = m.put_over(&mut n, 0, spilled.clone());
        abort.fetch_and(!gravity_abi::ctx::ABORT_BIT, AcqRel);
        assert!(stopped.is_err(), "a pending abort stops the put in the run's compact");
        assert!(n.stream.is_empty(), "nothing put");
        m.put_over(&mut n, 0, spilled).unwrap();
        assert_eq!(n.stream.len(), 1, "the run as it lay: the put with no abort puts it");
        let b = &n.stream[0];
        let (mut got, mut at) = (Vec::new(), 0); // alloc: Cold
        while at < b.len() {
            let rec = u32::from_ne_bytes(b[at + 32..at + 36].try_into().unwrap()) as usize;
            let (size, off) = (u32::from_ne_bytes(b[at + 16..at + 20].try_into().unwrap()) as usize, u32::from_ne_bytes(b[at + 20..at + 24].try_into().unwrap()) as usize);
            got.push(b[at + 16 + off + 4..][..size - 4].to_vec());
            at += rec;
        }
        let mut want = x.clone(); // alloc: Cold
        want.sort();
        assert_eq!(got, want, "every record once, in text order, with its copy");
    }

    /// D3027 (b): a plain sort's merge whose var runs would lay a run past its cap (a top's runs sealed by `n` rows, their
    /// copies past a region): no phase 2, the leader lays none, and the put after the pipeline sends every sealed run to the
    /// stream in its put form, each record with its copy; under the cap the same runs merge on the leader as before.
    #[test]
    fn a_merge_past_its_cap_puts_the_sealed_runs_instead() {
        let t = |w: &[&str]| w.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<Vec<u8>>>();
        let (x, y) = (t(&["pear", "apple and more", ""]), t(&["fig", "zucchini squash"]));
        let runs = [vrun(&x, 1), vrun(&y, 2)];
        let ps = partials(&runs);
        let over = |cap| Resident { cap, ..Resident::new(0..1, None, 0, Some(0)) };
        let node = || Mock { leader: vec![0; 1 << 15], stream: Vec::new(), oom: false, ends: Vec::new() }; // alloc: Cold
        let fold = |m: &Resident, n: &mut Mock| {
            for p in ps.iter() {
                m.merge_into(&mut State::new(&mut [], SinkRef::of(&mut Discard)), *p).unwrap();
            }
            m.finish(&mut State::new(bytes(&mut n.leader), SinkRef::of(&mut Discard))).unwrap();
        };
        let (m, mut n) = (over(RESIDENT_MAX), node());
        fold(&m, &mut n);
        vcheck(&n.leader, &[x.clone(), y.clone()]);
        assert!(m.spilled.lock().unwrap().is_empty() && n.stream.is_empty(), "under the cap: merged on the leader");
        let (m, mut n) = (over(64), node());
        assert!(m.partition_plan(&ps).is_none(), "no phase 2 past the cap");
        fold(&m, &mut n);
        assert_ne!(unsafe { (*(n.leader.as_ptr() as *const SortRun)).pad[0] }, RUN_SEALED, "the leader lays no run");
        let spilled = core::mem::take(&mut *m.spilled.lock().unwrap());
        assert_eq!(spilled.len(), 2, "both sealed runs left for the put");
        m.put_over(&mut n, 0, spilled).unwrap();
        assert_eq!(n.stream.len(), 2, "each sealed run put");
        let mut got = Vec::new(); // alloc: Cold
        for b in n.stream.iter() {
            let (mut run, mut at) = (Vec::new(), 0); // alloc: Cold
            while at < b.len() {
                let rec = u32::from_ne_bytes(b[at + 32..at + 36].try_into().unwrap()) as usize;
                let (size, off) = (u32::from_ne_bytes(b[at + 16..at + 20].try_into().unwrap()) as usize, u32::from_ne_bytes(b[at + 20..at + 24].try_into().unwrap()) as usize);
                run.push(b[at + 16 + off + 4..][..size - 4].to_vec());
                at += rec;
            }
            assert!(run.windows(2).all(|w| w[0] <= w[1]), "each put run in text order");
            got.extend(run);
        }
        got.sort();
        let mut want = [x, y].concat();
        want.sort();
        assert_eq!(got, want, "every record in the stream once, with its copy");
        assert_ne!(unsafe { (*(n.leader.as_ptr() as *const SortRun)).pad[0] }, RUN_SEALED, "no sealed run beside the stream");
    }
}
