//! The two-pass readers of `RunnerPlan` (admit set, pass-2 cut) and the fold of dictionary codes by value.
//! Split out of runner.rs (move-only, lane SPLIT 09-23).

use super::*;
use memledger::{ChargedVec, Kind, Owner};

mod raw;

impl RunnerPlan {
    /// The two-pass readers (§2.6 item 16): the declared `export` / `marks` over their words block (a two-pass plan is the
    /// declared `two`'s; no tier-3 hand kernel's by-identity reader remains).
    fn two_pass_readers(&self) -> Option<(gravity_program::ExportKeys, Option<gravity_program::GranuleMarks>, &[u64])> {
        self.two.map(|d| (d.export, d.marks, &self.params[d.base as usize..d.base as usize + d.words as usize]))
    }

    /// After pass 1: the exported keys counted and pass 2's claims cut to their granules (an unowned result: pass 2 reads
    /// every claim). Pass 2's admitted set is the protocol's (`hosts::ops::agg::hosts::admit_set`, sized from these keys).
    pub(super) fn build_admit_set(&self, exec: &mut Exec) -> gravity_driver::Result<()> {
        let Some(slot) = self.program.pipelines[self.pass1 as usize].produces() else { return Ok(()) };
        let owner = exec.node_mut().slot_owner(slot);
        let region = exec.node_mut().slot_region(slot);
        if !raw::is_owned(region) {
            return Ok(());
        }
        let mut keys: ChargedVec<u64> = ChargedVec::new(owner, Kind::Region); // alloc: Exec
        let Some((export, _, p1)) = self.two_pass_readers() else { return Ok(()) };
        raw::export_keys(export, region, p1, &mut |k| keys.with_vec(|v| v.push(k))); // alloc: Exec charged owner
        self.exported.set(keys.len() as u64);
        self.cut_pass2(region, &keys, owner);
        Ok(())
    }

    /// Pass 2's claims cut to the granules the admitted keys' marks name (`t3::gmark_lists`): an ordered
    /// walk over them in scan order, whole parts per claim, installed as pass 2's site's order for the
    /// section's binds; nothing when the marks overflowed, or name every granule, or none.
    fn cut_pass2(&self, region: &[u8], keys: &[u64], owner: Option<Owner>) {
        use gravity_kernels::ops::sort::topn::t3::{GMARK_ALL, GMARK_BITS};
        let Some((_, marks, p1)) = self.two_pass_readers() else { return };
        let Some(gmark_lists) = marks else { return };
        let Some(list) = self.resolved.list(self.pass2_rel) else { return };
        let total = list.granules() as usize;
        let mut admitted: ChargedVec<u64> = ChargedVec::new(owner, Kind::Region); // alloc: Exec charged owner
        admitted.with_vec(|v| v.extend_from_slice(keys)); // alloc: Exec charged owner
        admitted.sort_unstable();
        admitted.with_vec(|v| v.dedup());
        let mut bits: ChargedVec<u64> = ChargedVec::new(owner, Kind::Region); // alloc: Exec charged owner
        bits.with_vec(|v| v.resize(total.div_ceil(64), 0)); // alloc: Exec charged owner
        let mut set = |g: u64| bits[g as usize / 64] |= 1 << (g % 64);
        let mut all = false;
        let n = raw::granule_marks(gmark_lists, region, p1, &mut |fp, part, marks| {
            if part == GMARK_ALL {
                all = true;
                return;
            }
            if admitted.binary_search(&fp).is_err() || part as usize >= list.len() {
                return;
            }
            let (start, end) = (list.starts()[part as usize], list.end_of(part as usize));
            for b in 0..GMARK_BITS as u64 {
                if marks >> b & 1 == 0 {
                    continue;
                }
                if b == GMARK_BITS as u64 - 1 {
                    (start + b..end).for_each(&mut set);
                } else if start + b < end {
                    set(start + b);
                }
            }
        });
        if n.is_none() || all {
            return;
        }
        let order: Vec<u32> = (0..total as u32).filter(|&g| bits[g as usize / 64] >> (g % 64) & 1 != 0).collect(); // alloc: Exec
        self.admitted.set((order.len() as u64, total as u64));
        if order.is_empty() || order.len() == total {
            return;
        }
        let mut mins: ChargedVec<i64> = ChargedVec::new(owner, Kind::Region); // alloc: Exec charged owner
        mins.with_vec(|v| v.extend(order.iter().map(|&g| g as i64))); // alloc: Exec charged owner
        let mut cursor = GranuleRange::ordered(&order, &mins, |g| list.part_of(g as u64), ClaimSize::Adaptive { startup_c0: 1 });
        cursor.set_whole_boundary_claims(true);
        cursor.set_endgame_split(false);
        self.resolved.set_order(self.pass2_site, Some(Arc::from(order)));
        let _ = self.pass2.set(cursor);
    }
}

/// The two-level code aggregation's second level (§1.10 increment 3 step 4, T4): every `KeyKind::Code`
/// key rendered through the leader's source (the layout's code keys in key order are the site's code
/// lanes in lane order), its word replaced by the value datum; the records ordered by the keys (a code
/// key by its bytes, C collation), equal keys folded (`Rec::fold`), the table left `DENSE`.
pub(super) fn fold_codes_by_value(exec: &mut Exec, site: usize, params: &[u64]) -> gravity_driver::Result<()> {
    let node = exec.node_mut();
    let slot = node.program().pipelines[0].produces().unwrap_or(0);
    let owner = node.slot_owner(slot);
    let region = node.slot_region(slot);
    if region.len() < gravity_abi::breaker::HASHAGG_HDR {
        return Ok(());
    }
    let Some(table) = raw::agg_table(region.as_mut_ptr()) else { return Ok(()) };
    if table.cap == 0 {
        return Ok(());
    }
    let layout = table.layout;
    let mut lanes: ChargedVec<u16> = ChargedVec::new(owner, Kind::Region); // alloc: Cold charged owner
    lanes.with_vec(|v| v.resize(layout.nkeys(), u16::MAX)); // alloc: Cold charged owner
    let mut code_lanes = node.program().sites[site].lanes().iter().enumerate().filter(|(_, l)| l.kind == gravity_abi::ctx::lane::KIND_CODES).map(|(i, _)| i as u16);
    let mut any = false;
    for (k, kind) in layout.key_kinds().iter().enumerate() {
        if *kind == KeyKind::Code {
            lanes[k] = code_lanes.next().ok_or_else(|| gravity_driver::Error::host("gravity: more code keys than code lanes"))?;
            any = true;
        }
    }
    if !any {
        return Ok(());
    }
    let src = node.source_mut(site).ok_or_else(|| gravity_driver::Error::host("gravity: codes fold without a source"))?;
    for r in table.records() {
        for k in 0..layout.nkeys() {
            if lanes[k] == u16::MAX {
                continue;
            }
            let KeyCell::Code { part, code } = r.key_cell(k) else { unreachable!("gravity: a code key cell") };
            let (datum, _) = src.dict_value(part, lanes[k], code)?;
            r.set_key(k, datum);
        }
    }
    let bytes = raw::rendered_bytes;
    let cmp = |a: *const u8, b: *const u8| -> core::cmp::Ordering {
        for k in 0..layout.nkeys() {
            let off = layout.key_off(k);
            let (wa, wb) = (raw::record_word(a, off), raw::record_word(b, off));
            let o = if lanes[k] != u16::MAX { bytes(wa).cmp(bytes(wb)) } else { (wa as i64).cmp(&(wb as i64)) };
            if o != core::cmp::Ordering::Equal {
                return o;
            }
        }
        core::cmp::Ordering::Equal
    };
    table.sort_dense_by(&mut |a, b| cmp(a, b) == core::cmp::Ordering::Less);
    table.coalesce_dense(&mut |a, b| cmp(a, b) == core::cmp::Ordering::Equal).map_err(|_| gravity_driver::Error::Raised(gravity_abi::breaker::FloatOverflow::MASK))?;
    table.flags |= gravity_abi::breaker::FLAG_RENDERED;
    Ok(())
}
