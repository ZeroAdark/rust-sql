//! Key-column-first decode, late lanes for survivors (H01e, `decode.late`; R2-L11, the labs' "gathers at survivors only"
//! and "filter-first survivor gather"): when a pgrc pipeline's first op is a filter, the granule host decodes only the
//! lanes that filter reads, with the code and zone lanes it decodes first anyway; every other attribute lane the rows
//! read is late (`VERDICT_LATE`, the late lanes' mask in `src.verdicts` slot 3). The frame calls `fill_late` before
//! anything reads a late lane (M1-CORE f5b821ddc51): a block's survivors (the block path after step 0's mask, the general
//! body's tail) become a selection the host decodes each late lane for (`lane_fill` with the site's selection pointed at
//! it: ByteFor gathers, DeltaFor decodes up to the last selected row), and a row body fills the whole window through the
//! window's own selection. A window the host already made a selection (row_select, the condition cache's words, the PSMA
//! window, deletes) decoded its late lanes for that selection, and the fill is the host's no-op. No qual runs early, so
//! laziness and errors are the eager decode's. Registers road `decode.late`. Owner M1-SOURCE.
use core::cell::Cell;

use gravity_abi::ctx::{lane, site_off};
use gravity_abi::site::site;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{ConvOp, HostFlags, HostFn, IntBinOp, IntCond, IntUnOp, Type, Value};
use gravity_kernels::host_abi as abi;
use gravity_program::{LaneId, Rep, SiteDesc};
use mcx::{CollectIn, PgVec, GrowIn};
use types_nodes::Node;

use crate::face::strategy::TOPK_FETCH;
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::row::Mask;
use crate::ops::Table;

crate::strategies! {
    /// Key-column-first decode, late lanes for survivors.
    DECODE_LATE = 4, "decode.late", Pgrc;
}

pub(crate) const ROWS: Table = Table { strategies: &[(DECODE_LATE, &())], ..crate::ops::NONE };

/// The late lanes of pipeline `k`'s drafted `site` (bit l: site lane l): the attribute lanes no qual of the first op reads,
/// less the code lanes, the zone lanes and the elided lanes (the host decodes or skips those itself). None unless op 0
/// is a filter whose reads are known.
pub(crate) fn lanes(px: &Px<'_, '_>, k: usize, site: &SiteDesc) -> u64 {
    let p = &px.gx.pipelines()[k];
    let (Source::Range { scanrelid, .. }, Some(Op::Filter(q))) = (&p.source, p.ops.first()) else { return crate::dict::face::path_key(p.node_id).map_or(0, |f| (f.late)(px, k, site)) }; // C5 S4
    let n = site.natts().min(64);
    let (mut read, mut reads) = (0u64, [0u32; 64]);
    let mcx = px.mcx();
    let quals: PgVec<'_, Node<'_>> = match q.as_list() {
        Some(l) => l.iter().collect_in(mcx),
        None => mcx::pg_vec![in mcx; *q],
    };
    for q in quals {
        let mut whole = false;
        let known = gravity_decompose::expr::walk(q, &mut |x| {
            if let Some(v) = x.as_var().filter(|v| v.varno == *scanrelid as i32 && v.varlevelsup == 0) {
                whole |= v.varattno == 0;
                if let Some(l) = site.lanes()[..n].iter().position(|l| l.id == LaneId::Attr(v.varattno)) {
                    read |= 1 << l;
                    reads[l] += 1;
                }
            }
            true
        });
        if !known || whole {
            return 0;
        }
    }
    // M6 D2 / R3: a jsonb column only its path lanes' compares read is late, a filter lane or not (`SiteDesc::path_only`)
    let (any_read, po) = (read != 0, site.path_only(&reads[..n]));
    let mut late = 0u64;
    for (l, d) in site.lanes()[..n].iter().enumerate() {
        let host_first = d.kind == lane::KIND_CODES || site.zone_preds().iter().any(|z| z.lane as usize == l) || (site.filter_lanes.contains(l) && po >> l & 1 == 0);
        if matches!(d.id, LaneId::Attr(_)) && !host_first && read & !po & (1 << l) == 0 {
            late |= 1 << l;
        }
    }
    // a filter reading no lane of the site (a constant qual) leaves nothing to decode first
    if !any_read {
        return 0;
    }
    late
}

/// Road topk.fetch's scan (an ORDER BY .. LIMIT whose projection is wider than its keys; v2's `source/emit_fetch.rs` shape): when pipeline `k`'s top takes the fetch form
/// (runs/fetch.rs `of`, restated: that module is M3's), a column only its projection reads, not a sort key, leaves the decomposer's `site`: the run keeps
/// `(key, locator)` alone and the fetch reads that column for the winners, so no granule decodes or reads it for a
/// candidate (step/project.rs passes its bare entry on as no cell). Every column a filter, a gate, a computed entry or a
/// key reads keeps its lane. None narrowed unless the ops before the sort are filters and gates then one projection, every
/// read known and no whole row. The derived lanes (offset, rowid) follow the attributes, their bits renumbered, and so does a
/// code lane's `DictRef` (D4086: a stale one decoded another lane's codes); none narrowed where a table, shred or OR zone names a lane.
pub(crate) fn fetch_narrow(px: &Px<'_, '_>, k: usize, site: SiteDesc) -> SiteDesc {
    let p = &px.gx.pipelines()[k];
    // a merged site keeps its key lanes (sortmerge)
    if site.is_merged() {
        return site;
    }
    let Source::Range { scanrelid, .. } = &p.source else { return site };
    let Some(j) = p.ops.iter().position(|op| matches!(op, Op::Sort(_))) else { return site };
    let (Some(Op::Project(list)), Some(Op::Sort(sd))) = (j.checked_sub(1).map(|i| &p.ops[i]), p.ops.get(j)) else { return site };
    // runs/fetch.rs `of`: the road on and the wide top's shape over this projection
    let road = px.strategies.has(TOPK_FETCH) && crate::face::source::on(crate::admit::refusal::Variant::Strategy(TOPK_FETCH));
    // a rerun set member's top keeps full rows (`of`'s own test): its site keeps every lane
    if !road || !crate::props::topk_wide(k, p, sd, Some(list), px.mcx()) || crate::rerun::sets::in_owner_set(px.gx, k) {
        return site;
    }
    let mcx = px.mcx();
    let entries: PgVec<'_, Node<'_>> = list.iter().map(|n| n.as_target_entry().map_or(n, |t| t.expr)).collect_in(mcx);
    // each key's input entry: the Sort's OUTER_VAR target names the entry by position (props/topk.rs `word_key`)
    let mut keep = mcx::pg_vec![in mcx; false; entries.len()];
    for key in sd.keys.iter() {
        let at = sd.tlist.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == key.resno).and_then(|te| te.expr.as_var());
        let Some(e) = at.and_then(|v| (v.varattno as usize).checked_sub(1)).filter(|&e| e < entries.len()) else { return site };
        keep[e] = true;
    }
    let mut read: PgVec<'_, i16> = PgVec::new_in(mcx);
    let mut reads = |n: Node<'_>| {
        let mut whole = false;
        let known = gravity_decompose::expr::walk(n, &mut |x| {
            if let Some(v) = x.as_var().filter(|v| v.varno == *scanrelid as i32 && v.varlevelsup == 0) {
                whole |= v.varattno <= 0;
                read.push_in(v.varattno);
            }
            true
        });
        known && !whole
    };
    for op in &p.ops[..j - 1] {
        let (Op::Filter(q) | Op::Gate(q)) = op else { return site };
        if !reads(*q) {
            return site;
        }
    }
    for (e, &n) in entries.iter().enumerate() {
        let bare = n.as_var().is_some_and(|v| v.varno == *scanrelid as i32 && v.varlevelsup == 0 && v.varattno >= 1);
        if (keep[e] || !bare) && !reads(n) {
            return site;
        }
    }
    let natts = site.natts();
    let kept: PgVec<'_, usize> = (0..site.nlanes()).filter(|&l| l >= natts || matches!(site.lanes()[l].id, LaneId::Attr(a) if read.contains(&a))).collect_in(mcx);
    if kept.len() == site.nlanes() || kept.iter().all(|&l| l >= natts) || site.entries.is_some() || site.out_attnos.len() != natts || !site.tables.is_empty() || !site.shreds.is_empty() || !site.zone_any.is_empty() {
        return site;
    }
    let mut s = site.copy_in(px.arena);
    (s.lanes.clear(), s.out_attnos.clear(), s.driving_lanes.clear(), s.filter_lanes.clear(), s.answer_lanes.clear());
    s.clear_zone();
    for (to, &l) in kept.iter().enumerate() {
        let mut d = site.lanes()[l];
        if let Rep::Code { dict } = &mut d.rep {
            let Some(at) = kept.iter().position(|&m| m == dict.lane as usize) else { return site };
            dict.lane = at as u8;
        }
        s.add_lane(d, site.driving_lanes.contains(l));
        if site.filter_lanes.contains(l) {
            s.filter_lanes.insert(to);
        }
        if site.answer_lanes.contains(l) {
            s.answer_lanes.insert(to);
        }
        if l < natts {
            s.out_attnos.push(site.out_attnos[l]); // alloc: Program
        }
    }
    let nkeys = site.nkeys().max(gravity_program::RANGE_KEYS) as u16;
    let zkeys = match site.kind {
        gravity_program::SiteKind::RangeSource { zkeys, .. } => zkeys,
        _ => 0,
    };
    s.kind = gravity_program::SiteKind::RangeSource { natts: s.out_attnos.len() as u16, nkeys, zkeys };
    s
}

/// `DecodeOrder::Late`'s `first`: the site's non-late attribute lanes, by `WordWin` lane (attno - 1).
pub(crate) fn first(site: &SiteDesc, late: u64) -> u64 {
    let mut first = 0u64;
    for (l, d) in site.lanes().iter().enumerate().take(64) {
        if let LaneId::Attr(a) = d.id {
            if late & (1 << l) == 0 && (1..=64).contains(&a) {
                first |= 1 << (a - 1);
            }
        }
    }
    first
}

/// A pgrc source's late lanes and its `lane_fill` host: `mask` the fill set, `block` a whole block's (less the packed
/// candidates the first step alone reads).
pub(crate) struct Late {
    pub mask: u64,
    pub block: u64,
    host: Cell<Option<HostFn>>,
}

impl Late {
    pub(crate) fn new(mask: u64, block: u64) -> Late {
        Late { mask, block, host: Cell::new(None) }
    }

    pub(crate) fn declare(&self, pb: &mut ProgramBuilder) {
        if self.mask != 0 {
            let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
            self.host.set(Some(pb.declare_host("lane_fill", &[Type::Ptr, Type::I64, Type::I64], Some(Type::I64), gravity_kernels::contract::entry2::<abi::LaneFill> as *const () as usize, mem)));
        }
    }

    /// `lane_fill(site s, k)`'s status (a lane index with its flags); a negative one leaves through `l_exit`.
    pub(crate) fn call(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, s: usize, k: u32) -> Value {
        let Some(h) = self.host.get() else { return env.zero };
        let (sv, kv) = (b.iconst(Type::I64, s as i64), b.iconst(Type::I64, k as i64));
        let st = b.call_host(h, &[env.ctx, sv, kv], Some(Type::I64)).unwrap_or(env.zero);
        let neg = b.icmp(IntCond::Slt, st, env.zero);
        let l_ok = b.create_block(&[]);
        b.cond_br(neg, (env.x.l_exit, &[st]), (l_ok, &[]));
        b.switch_to(l_ok);
        st
    }

    /// Site `s`'s late lanes in `mask` decoded for the rows `at` names (block `base`'s rows in `m`, the site's selection pointed at
    /// them; none for an empty `m`, which an empty selection would read as every row), or for the window's own selection;
    /// a failing fill leaves through `l_exit` with its status.
    pub(crate) fn fill(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, s: usize, at: Option<(Value, Mask)>, mask: u64) {
        if self.host.get().is_none() || mask == 0 {
            return;
        }
        let so = site_off(s) as i32;
        let l_end = at.map(|(_, m)| {
            let (l_go, l_end) = (b.create_block(&[]), b.create_block(&[]));
            let none = b.icmp(IntCond::Eq, m.0, env.zero);
            b.cond_br(none, (l_end, &[]), (l_go, &[]));
            b.switch_to(l_go);
            l_end
        });
        let saved = at.map(|(base, m)| {
            let slot = b.declare_stack_slot(64 * 4, 4);
            let sel = b.stack_addr(slot);
            let (l_head, l_one, l_done) = (b.create_block(&[Type::I64, Type::I64]), b.create_block(&[]), b.create_block(&[Type::I64]));
            b.br(l_head, &[m.0, env.zero]);
            // at most 64 turns (one per survivor of the block), inside the checked row loop
            env.bounded.push_in(l_head);
            b.switch_to(l_head);
            let (bits, n) = (b.block_params(l_head)[0], b.block_params(l_head)[1]);
            let none = b.icmp(IntCond::Eq, bits, env.zero);
            b.cond_br(none, (l_done, &[n]), (l_one, &[]));
            b.switch_to(l_one);
            let t = b.int_un(IntUnOp::Ctz, bits);
            let row = b.int_bin(IntBinOp::Add, base, t);
            let row = b.conv(ConvOp::Trunc, Type::I32, row);
            let at = b.ptr_add(sel, n, 4, 0);
            b.store(at, 0, row);
            let low = b.int_bin(IntBinOp::Sub, bits, env.one);
            let rest = b.int_bin(IntBinOp::And, bits, low);
            let n1 = b.int_bin(IntBinOp::Add, n, env.one);
            b.br(l_head, &[rest, n1]);
            b.switch_to(l_done);
            let n = b.block_params(l_done)[0];
            let old = (b.load(Type::Ptr, env.ctx, so + site::SEL as i32), b.load(Type::I32, env.ctx, so + site::SEL_N as i32));
            b.store(env.ctx, so + site::SEL as i32, sel);
            let n32 = b.conv(ConvOp::Trunc, Type::I32, n);
            b.store(env.ctx, so + site::SEL_N as i32, n32);
            old
        });
        let mut bits = mask;
        while bits != 0 {
            let k = bits.trailing_zeros();
            bits &= bits - 1;
            self.call(b, env, s, k);
        }
        if let Some((sel, n)) = saved {
            b.store(env.ctx, so + site::SEL as i32, sel);
            b.store(env.ctx, so + site::SEL_N as i32, n);
        }
        if let Some(l_end) = l_end {
            b.br(l_end, &[]);
            b.switch_to(l_end);
        }
    }
}
