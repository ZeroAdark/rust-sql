//! D3027 (B), `code.heads`' count pass (ProtoKind::Heads; lowerv4 dict/strategies/heads_pass.rs): the pass between the
//! top (`peer`) and its emit counts, on the pool, each part the top's run names, one whole part per claim (kernels heads
//! `heads_tally`), into a directory behind the proto's word that the emit's `heads_count` reads before its own count.
//! At prepare: below width 2 the pass is pruned whole and the word is null (the emit counts on its thread, as before);
//! else the directory, one entry per part of the execution's part list, `[n, (first granule, slots, table) x n]` (first
//! granules ascending). After the top: the parts its sealed run's marks name (the leader's merged run, resident at W > 1
//! since D3027 (b)) are the pass's claims, every other part dead; a run not sealed there (put to the stream) names none
//! and the pass claims nothing (the emit counts as without it). code.heads is refused in rerun sets (lowerv4 topk_rows.rs
//! `D_TOP_RERUN`), so the top runs once per execution and the directory never goes stale. D4554 (the `heads_cache` switch,
//! off by default): a part keeps the rows of each code its statements' emits asked (pgrc_read `OpenPart::code_counts`:
//! its rows never change, and the top marks only delete-free parts), so after the top a marked part that keeps every code
//! its marks name gets a sparse table of them in its entry (kernels `PASS_SPARSE`) and no claim; after the pass each
//! counted part keeps its marked codes' rows. A hot repeat counts nothing.
use core::cell::{OnceCell, UnsafeCell};
use std::sync::Arc;

use gravity_driver::Exec;
use gravity_hosts::PartList;
use gravity_kernels::dict::strategies::heads::{PASS_DIR_ENTRY, PASS_DIR_HDR, PASS_NO_TABLE, PASS_SPARSE};
use gravity_parallel::{ClaimSize, GranuleRange, MorselSource};
use gravity_program::protos::{Proto, ProtoKind, NONE, NO_SLOT};
use memledger::{ChargedVec, Kind};
use types_error::PgResult;
use types_storage::lock::NoLock;

use crate::glue;
use crate::ops::{Live, Prepare, Protocol};

pub(crate) struct Heads;

struct HeadsLive {
    /// The count pass and the top it follows (program order).
    pass: usize,
    top: usize,
    /// The top's breaker slot and the mark word's offset in its run row (`Proto::arg`).
    slot: u8,
    mark_off: usize,
    /// Below width 2: the pass skipped whole, the directory word null.
    pruned: bool,
    list: Option<Arc<PartList>>,
    /// The directory the kernels write through the word (one writer per part entry: the pass's claims are whole parts),
    /// held for the execution.
    dir: Box<[UnsafeCell<u64>]>,
    cut: OnceCell<GranuleRange>,
    /// D4554 (`heads_cache` on, the switch read at prepare): the key's column, the (part first granule, code) pairs the
    /// top's marks name, and the sparse tables filed from the parts' kept counts (their entries point into it).
    cache: Option<i16>,
    marked: OnceCell<ChargedVec<(u64, u32)>>,
    _sparse: OnceCell<ChargedVec<u64>>,
}

impl Protocol for Heads {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Heads
    }
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let (pass, base) = (p.pipeline as usize, p.words.base as usize);
        if p.slot == NO_SLOT || p.peer == NONE || p.words.n != 1 || base >= cx.words.len() || base >= cx.nulls.len() {
            return Err(glue::internal("gravity: code.heads' count pass without its top, slot or directory word"));
        }
        let pruned = cx.widths.get(pass).is_none_or(|&w| w < 2);
        let (list, cache) = match pruned {
            true => (None, None),
            false => {
                let Some(g) = usize::try_from(p.site).ok().and_then(|s| cx.admitted.registry.granule_recipe(s)) else {
                    return Err(glue::internal("gravity: code.heads' count pass on a site without a pgrc granule recipe"));
                };
                let rel = table::table_open(cx.mcx, g.rel, NoLock)?;
                let list = gravity_hosts::resolve(&rel, Some(cx.snap));
                rel.close(NoLock)?;
                (Some(list?), g.attnos.first().copied().filter(|_| mempolicy::keep::heads_cache()))
            }
        };
        let starts = list.as_ref().map_or(&[][..], |l| l.starts());
        let mut words = vec![0u64; PASS_DIR_HDR + starts.len() * PASS_DIR_ENTRY]; // alloc: Exec
        words[0] = starts.len() as u64;
        for (i, &s) in starts.iter().enumerate() {
            words[PASS_DIR_HDR + i * PASS_DIR_ENTRY] = s;
        }
        let dir: Box<[UnsafeCell<u64>]> = words.into_iter().map(UnsafeCell::new).collect(); // alloc: Exec
        (cx.words[base], cx.nulls[base]) = (if pruned { 0 } else { dir.as_ptr() as u64 }, 0);
        let live = HeadsLive { pass, top: p.peer as usize, slot: p.slot, mark_off: p.arg as usize, pruned, list, dir, cut: OnceCell::new(), cache, marked: OnceCell::new(), _sparse: OnceCell::new() };
        Ok(Some(Box::new(live))) // alloc: Exec
    }
}

impl Live for HeadsLive {
    fn pruned_whole(&self, pipeline: usize) -> bool {
        self.pruned && pipeline == self.pass
    }
    fn claims_for(&self, pipeline: usize) -> Option<&dyn MorselSource> {
        self.cut.get().filter(|_| pipeline == self.pass).map(|c| c as &dyn MorselSource)
    }
    /// After the top: the pass's claims, the parts its run's marks name (none when its run is not sealed in the region)
    /// but those whose kept counts answer every code their marks name (D4554). After the pass: the counted parts keep
    /// their marked codes' rows (D4554). `None`: the runner's own after-pipeline steps still run.
    fn after_pipeline(&self, pipeline: usize, exec: &mut Exec) -> Option<gravity_driver::Result<()>> {
        if pipeline == self.pass && !self.pruned {
            self.keep_counted(exec.node_mut().slot_owner(self.slot));
            return None;
        }
        let list = self.list.as_ref().filter(|_| pipeline == self.top && !self.pruned)?;
        let total = list.granules();
        let owner = exec.node_mut().slot_owner(self.slot);
        let named = super::raw::marked_starts(exec.node_mut().slot_region(self.slot), self.mark_off, owner).unwrap_or_default();
        let mut live = ChargedVec::new(owner, Kind::Region); // alloc: Exec charged owner
        live.with_vec(|v| v.resize(list.len(), false)); // alloc: Exec charged owner
        for &s in named.iter() {
            let p = list.part_of(s);
            if list.starts().get(p) == Some(&s) {
                live[p] = true;
            }
        }
        if let Some(attno) = self.cache {
            self.file_kept(list, attno, exec, owner, &mut live);
        }
        let mut dead: ChargedVec<(u64, u64)> = ChargedVec::new(owner, Kind::Region); // alloc: Exec charged owner
        dead.with_vec(|v| v.extend((0..list.len()).filter(|&p| !live[p]).map(|p| (list.starts()[p], list.end_of(p))).filter(|d| d.0 < d.1))); // alloc: Exec charged owner
        if live.iter().any(|&l| l) {
            fires::fire("heads.parts.section"); // D3027 (B): the count pass counts the top's parts on the pool
        }
        let mut c = GranuleRange::over_parts(total, ClaimSize::Adaptive { startup_c0: 1 }, list.starts());
        c.set_whole_boundary_claims(true);
        // one writer per part entry at any width: the runtime's end-game split must not cut a part
        c.set_endgame_split(false);
        c.set_dead(&dead);
        // a part no claim counts keeps `slots` 0 in its entry: the emit counts its entries itself (kernels `counted`)
        let _ = self.cut.set(c);
        None
    }
}

impl HeadsLive {
    /// D4554, after the top: the marked parts whose kept counts answer every code their marks name get a sparse table of
    /// them in their entry, and are no claim of the pass (`live` false).
    fn file_kept(&self, list: &PartList, attno: i16, exec: &mut Exec, owner: Option<memledger::Owner>, live: &mut [bool]) {
        let Some(marked) = super::raw::marked_codes(exec.node_mut().slot_region(self.slot), self.mark_off, owner) else { return };
        let mut words: ChargedVec<u64> = ChargedVec::new(owner, Kind::Region); // alloc: Exec charged owner
        let mut filed: ChargedVec<(usize, usize, usize)> = ChargedVec::new(owner, Kind::Region); // alloc: Exec charged owner
        let mut rows: ChargedVec<u32> = ChargedVec::new(owner, Kind::Region); // alloc: Exec charged owner
        let mut asked: ChargedVec<u32> = ChargedVec::new(owner, Kind::Region); // alloc: Exec charged owner
        // bounded("other: one step per part the top's marks name")
        for run in marked.chunk_by(|a, b| a.0 == b.0) {
            let p = list.part_of(run[0].0);
            let spec = &list.parts()[p];
            let (Some(part), None) = (spec.part.as_ref(), spec.deletes.as_ref()) else { continue };
            if list.starts()[p] != run[0].0 || !live[p] {
                continue;
            }
            rows.with_vec(|v| {
                v.clear();
                v.resize(run.len(), 0); // alloc: Exec charged owner
            });
            asked.with_vec(|v| {
                v.clear();
                v.extend(run.iter().map(|&(_, c)| c)); // alloc: Exec charged owner
            });
            if !part.code_counts(attno, &asked, &mut rows) {
                continue;
            }
            let at = words.len();
            words.with_vec(|v| v.extend(asked.iter().zip(rows.iter()).map(|(&c, &r)| (c as u64) << 32 | r as u64))); // alloc: Exec charged owner
            filed.with_vec(|v| v.push((p, at, run.len()))); // alloc: Exec charged owner
            live[p] = false;
        }
        if !filed.is_empty() {
            fires::fire("heads.cache.hit"); // D4554: a marked part's kept counts answer its emit (no count-pass claim)
            // the words stay put in `_sparse` for the execution, and no claim counts a sparse part: its entry stays the
            // protocol's
            let base = words.as_ptr() as u64;
            // bounded("other: one step per part the top's marks name")
            for &(p, at, n) in filed.iter() {
                super::raw::set_pass_entry(&self.dir, p, PASS_SPARSE | n as u64, base + 8 * at as u64);
            }
            let _ = self._sparse.set(words);
        }
        let _ = self.marked.set(marked);
    }

    /// D4554, after the pass: each part the pass counted into a table keeps its marked codes' rows.
    fn keep_counted(&self, owner: Option<memledger::Owner>) {
        let (Some(list), Some(attno), Some(marked)) = (self.list.as_ref(), self.cache, self.marked.get()) else { return };
        let mut pairs: ChargedVec<(u32, u32)> = ChargedVec::new(owner, Kind::Region); // alloc: Exec charged owner
        // bounded("other: one step per part the top's marks name")
        for run in marked.chunk_by(|a, b| a.0 == b.0) {
            let p = list.part_of(run[0].0);
            let spec = &list.parts()[p];
            let (Some(part), None) = (spec.part.as_ref(), spec.deletes.as_ref()) else { continue };
            if list.starts()[p] != run[0].0 {
                continue;
            }
            // the pass has ended: no worker writes the directory now
            let (slots, t) = super::raw::pass_entry(&self.dir, p);
            if slots == 0 || slots == PASS_NO_TABLE || slots & PASS_SPARSE != 0 || t == 0 {
                continue;
            }
            pairs.with_vec(|v| {
                v.clear();
                v.extend(run.iter().map(|&(_, c)| (c, super::raw::pass_rows(t, slots, c)))); // alloc: Exec charged owner
            });
            part.keep_code_counts(attno, &pairs);
        }
    }
}
