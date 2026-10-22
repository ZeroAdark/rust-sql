// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#[path = "analyzer_guard.rs"]
mod analyzer_guard;
use crate::ir_adaptor::{HighestValuePolicy, IrAdaptor, PhiRef};
use analyzer_guard::MetadataRestore;
use core::{fmt, marker::PhantomData};
use mcx::{ForgetSafe, Mcx};
use tpde_base::ValLocalIdx;
use tpde_util::{tpde_assert, tpde_assert_eq, SmallBitSet, SmallVec, SmallVectorGuard};
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};

pub const SMALL_BLOCK_NUM: usize = 64;
pub const SMALL_VALUE_NUM: usize = 128;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct BlockIndex(pub u32);
pub const INVALID_BLOCK_IDX: BlockIndex = BlockIndex(u32::MAX);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Loop {
    pub level: u32,
    pub parent: u32,
    pub begin: BlockIndex,
    pub end: BlockIndex,
    pub num_blocks: u32,
    pub definitions: u32,
    pub definitions_in_childs: u32,
}
impl Loop {
    fn new(level: u32, parent: u32) -> Self {
        Self {
            level,
            parent,
            begin: INVALID_BLOCK_IDX,
            end: INVALID_BLOCK_IDX,
            num_blocks: 0,
            definitions: 0,
            definitions_in_childs: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct LivenessInfo {
    pub first: BlockIndex,
    pub last: BlockIndex,
    pub ref_count: u32,
    pub lowest_common_loop: u32,
    pub last_full: bool,
    pub epoch: u16,
    /// Static spill weight: one unit per visit (definition, use, phi edge)
    /// scaled by `4^loop_level` of the visiting block, saturating. A
    /// frontend may override it through the compiler target.
    pub weight: u32,
    /// Some block in `[first, last]` contains a hot call
    /// (`IrAdaptor::block_calls`), so a caller-saved register cannot
    /// hold the value for its whole range.
    pub crosses_call: bool,
    /// Some block in `[first, last]` contains a cold call: a caller-saved
    /// register may hold the value if the call sequence saves and
    /// restores it, and it must not be an argument or return register of
    /// that call.
    pub crosses_cold_call: bool,
}
#[derive(Clone, Copy)]
pub struct LivenessTable<'a> {
    infos: &'a [LivenessInfo],
    epoch: u16,
}
impl<'a> LivenessTable<'a> {
    #[inline]
    pub fn info(&self, idx: ValLocalIdx) -> &'a LivenessInfo {
        let info = &self.infos[idx.get() as usize];
        tpde_assert!(
            self.epoch != 0 && info.epoch == self.epoch,
            "access to ignored or invalid liveness"
        );
        info
    }
}
impl LivenessInfo {
    fn extend_block(&mut self, block: u32, backwards: bool) {
        let old_last = self.last.0;
        self.first.0 = self.first.0.min(block);
        self.last.0 = self.last.0.max(block);
        if old_last == self.last.0 && backwards {
            self.last_full = true;
        } else if old_last != self.last.0 {
            self.last_full = false;
        }
    }
    fn extend_loop(&mut self, loop_info: Loop, backwards: bool) {
        let old_last = self.last.0;
        self.first.0 = self.first.0.min(loop_info.begin.0);
        self.last.0 = self.last.0.max(loop_info.end.0 - 1);
        if old_last != self.last.0 || backwards {
            self.last_full = true;
        }
    }
}

#[derive(Clone, Copy, Default)]
struct BlockInfo {
    traversed: bool,
    self_loop: bool,
    dfsp_pos: u32,
    iloop_header: u32,
}
mcx::forget_safe_nodrop!(BlockIndex, Loop, LivenessInfo, BlockInfo);
const _: () = assert!(core::mem::size_of::<Loop>() <= 128);
const _: () = assert!(core::mem::size_of::<LivenessInfo>() <= 128);

struct Traversal<I> {
    block_idx: u32,
    dfsp_pos: u32,
    successors: I,
}

#[doc = include_str!("../tests/analyzer-ownership.md")]
pub struct Analyzer<'m, A: IrAdaptor>
where
    A::BlockRef: Copy + ForgetSafe,
{
    pub block_layout: SmallVec<'m, A::BlockRef, SMALL_BLOCK_NUM>,
    pub block_loop_map: SmallVec<'m, u32, SMALL_BLOCK_NUM>,
    pub loops: SmallVec<'m, Loop, 16>,
    pub liveness: SmallVec<'m, LivenessInfo, SMALL_VALUE_NUM>,
    pub liveness_epoch: u16,
    pub liveness_max_value: u32,
    pub num_insts: u32,
    block_rpo: SmallVec<'m, A::BlockRef, SMALL_BLOCK_NUM>,
    rpo_stack: SmallVec<'m, A::BlockRef, 32>,
    loop_parent: SmallVec<'m, u32, SMALL_BLOCK_NUM>,
    loop_heads: SmallBitSet<'m, 4>,
    block_infos: SmallVec<'m, BlockInfo, SMALL_BLOCK_NUM>,
    block_loops: SmallVec<'m, u32, SMALL_BLOCK_NUM>,
    /// `calls_before[i]` = number of layout blocks `< i` that contain a hot call.
    calls_before: SmallVec<'m, u32, SMALL_BLOCK_NUM>,
    /// Same, for cold calls.
    cold_calls_before: SmallVec<'m, u32, SMALL_BLOCK_NUM>,
    chain: SmallVec<'m, u32, 32>,
    /// D3651: the blocks the layout sank (`IrAdaptor::block_sink`): RPO
    /// indices while the layout is built, then their layout indices.
    sunk: SmallVec<'m, u32, 16>,
    /// `extend_through_sunk`: (successor, sunk block) layout index pairs,
    /// sorted, and the range-maximum table over their sunk indices.
    sunk_edges: SmallVec<'m, (u32, u32), 16>,
    sunk_reach: SmallVec<'m, u32, 16>,
    saved_metadata: SmallVec<'m, (A::BlockRef, u32, u32), SMALL_BLOCK_NUM>,
    diagnostic_rpo: SmallVec<'m, A::BlockRef, SMALL_BLOCK_NUM>,
    mcx: Mcx<'m>,
    marker: PhantomData<fn() -> A>,
}

#[cold]
fn invalid(message: &'static str) -> PgError {
    PgError::error(message).with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)
}
fn ensure(ok: bool, message: &'static str) -> PgResult<()> {
    if ok {
        Ok(())
    } else {
        Err(invalid(message).into())
    }
}

impl<'m, A: IrAdaptor> Analyzer<'m, A>
where
    A::BlockRef: Copy + ForgetSafe,
    A::ValueRef: Copy,
    A::InstRef: Copy,
{
    pub const INVALID_BLOCK_REF: A::BlockRef = A::INVALID_BLOCK_REF;
    pub const INVALID_BLOCK_IDX: BlockIndex = INVALID_BLOCK_IDX;
    pub const SMALL_BLOCK_NUM: usize = SMALL_BLOCK_NUM;
    pub const SMALL_VALUE_NUM: usize = SMALL_VALUE_NUM;

    pub fn new(mcx: Mcx<'m>) -> Self {
        const {
            assert!(
                core::mem::size_of::<A::ValueRef>() <= 8,
                "IR value reference exceeds 8 bytes"
            );
            assert!(
                core::mem::size_of::<A::BlockRef>() <= 8,
                "IR block reference exceeds 8 bytes"
            );
            assert!(
                core::mem::size_of::<A::FuncRef>() <= 8,
                "IR function reference exceeds 8 bytes"
            );
        }
        Self {
            block_layout: SmallVec::new(mcx),
            block_loop_map: SmallVec::new(mcx),
            loops: SmallVec::new(mcx),
            liveness: SmallVec::new(mcx),
            liveness_epoch: 0,
            liveness_max_value: 0,
            num_insts: 0,
            block_rpo: SmallVec::new(mcx),
            rpo_stack: SmallVec::new(mcx),
            loop_parent: SmallVec::new(mcx),
            loop_heads: SmallBitSet::new(mcx),
            block_infos: SmallVec::new(mcx),
            block_loops: SmallVec::new(mcx),
            calls_before: SmallVec::new(mcx),
            cold_calls_before: SmallVec::new(mcx),
            chain: SmallVec::new(mcx),
            sunk: SmallVec::new(mcx),
            sunk_edges: SmallVec::new(mcx),
            sunk_reach: SmallVec::new(mcx),
            saved_metadata: SmallVec::new(mcx),
            diagnostic_rpo: SmallVec::new(mcx),
            mcx,
            marker: PhantomData,
        }
    }

    pub fn switch_func(&mut self, adaptor: &A, _func: A::FuncRef) -> PgResult<()> {
        let result = self
            .build_block_layout(adaptor)
            .and_then(|()| self.compute_liveness(adaptor));
        if result.is_err() {
            for block in adaptor.cur_blocks() {
                adaptor.block_set_info(block, u32::MAX);
                adaptor.block_set_info2(block, 0);
            }
            self.block_layout.clear();
            self.block_loop_map.clear();
            self.loops.clear();
            self.liveness.clear();
            self.liveness_epoch = 0;
            self.liveness_max_value = 0;
            self.num_insts = 0;
        }
        result
    }

    pub fn block_ref(&self, idx: BlockIndex) -> A::BlockRef {
        if idx.0 as usize == self.block_layout.len() {
            A::INVALID_BLOCK_REF
        } else {
            self.block_layout[idx.0 as usize]
        }
    }
    pub fn block_idx(&self, adaptor: &A, block: A::BlockRef) -> BlockIndex {
        BlockIndex(adaptor.block_info(block))
    }
    pub fn liveness_info(&self, idx: ValLocalIdx) -> &LivenessInfo {
        self.liveness_table().info(idx)
    }
    /// The liveness vector resolved once, for loops that look up many values
    /// (upstream indexes the SmallVector directly); every lookup keeps the
    /// epoch check.
    #[inline]
    pub fn liveness_table(&self) -> LivenessTable<'_> {
        LivenessTable {
            infos: &self.liveness,
            epoch: self.liveness_epoch,
        }
    }
    pub fn block_loop_idx(&self, idx: BlockIndex) -> u32 {
        self.block_loop_map[idx.0 as usize]
    }
    pub fn loop_from_idx(&self, idx: u32) -> &Loop {
        &self.loops[idx as usize]
    }
    pub fn block_has_multiple_incoming(&self, adaptor: &A, idx: BlockIndex) -> bool {
        self.block_ref_has_multiple_incoming(adaptor, self.block_ref(idx))
    }
    pub fn block_ref_has_multiple_incoming(&self, adaptor: &A, block: A::BlockRef) -> bool {
        adaptor.block_info2(block) & 3 == 2
    }
    pub fn block_has_phis(&self, adaptor: &A, idx: BlockIndex) -> bool {
        self.block_ref_has_phis(adaptor, self.block_ref(idx))
    }
    pub fn block_ref_has_phis(&self, adaptor: &A, block: A::BlockRef) -> bool {
        adaptor.block_info2(block) & 16 != 0
    }

    pub fn print_rpo(&mut self, adaptor: &A, out: &mut impl fmt::Write) -> PgResult<()> {
        self.saved_metadata.clear();
        for block in adaptor.cur_blocks() {
            self.saved_metadata.push((
                block,
                adaptor.block_info(block),
                adaptor.block_info2(block),
            ))?;
        }
        let _restore = MetadataRestore {
            adaptor,
            saved: &self.saved_metadata,
        };
        Self::rpo(adaptor, &mut self.diagnostic_rpo, &mut self.rpo_stack).and_then(|()| {
            for (idx, block) in self.diagnostic_rpo.iter().enumerate() {
                writeln!(out, "  {}: {}", idx, adaptor.block_fmt_ref(*block))
                    .map_err(|_| invalid("analyzer diagnostic output failed"))?;
            }
            Ok(())
        })
    }
    pub fn print_block_layout(&self, adaptor: &A, out: &mut impl fmt::Write) -> fmt::Result {
        for (idx, block) in self.block_layout.iter().enumerate() {
            writeln!(out, "  {}: {}", idx, adaptor.block_fmt_ref(*block))?;
        }
        Ok(())
    }
    pub fn print_loops(&self, _adaptor: &A, out: &mut impl fmt::Write) -> fmt::Result {
        for (idx, info) in self.loops.iter().enumerate() {
            writeln!(
                out,
                "  {}: level {}, parent {}, {}->{}",
                idx, info.level, info.parent, info.begin.0, info.end.0
            )?;
        }
        Ok(())
    }
    pub fn print_liveness(&self, adaptor: &A, out: &mut impl fmt::Write) -> fmt::Result {
        for idx in 0..=self.liveness_max_value {
            let Some(info) = self
                .liveness
                .get(idx as usize)
                .filter(|info| self.liveness_epoch != 0 && info.epoch == self.liveness_epoch)
            else {
                writeln!(out, "  {}: ignored", idx)?;
                continue;
            };
            writeln!(
                out,
                "  {}: {} refs, {}->{} ({}->{}), lf: {}",
                idx,
                info.ref_count,
                info.first.0,
                info.last.0,
                adaptor.block_fmt_ref(self.block_ref(info.first)),
                adaptor.block_fmt_ref(self.block_ref(info.last)),
                info.last_full
            )?;
        }
        Ok(())
    }

    fn build_block_layout(&mut self, adaptor: &A) -> PgResult<()> {
        Self::rpo(adaptor, &mut self.block_rpo, &mut self.rpo_stack)?;
        self.identify_loops(adaptor)?;
        self.loop_heads.mark_set(0)?;
        self.build_loop_tree_and_block_layout(adaptor)
    }

    fn rpo(
        adaptor: &A,
        out: &mut SmallVec<'m, A::BlockRef, SMALL_BLOCK_NUM>,
        stack: &mut SmallVec<'m, A::BlockRef, 32>,
    ) -> PgResult<()> {
        out.clear();
        let mut count = 0u32;
        let mut found_entry = false;
        for block in adaptor.cur_blocks() {
            adaptor.block_set_info(block, count);
            adaptor.block_set_info2(block, 0);
            count = count
                .checked_add(1)
                .ok_or_else(|| invalid("too many blocks"))?;
            found_entry |= block == adaptor.cur_entry_block();
        }
        ensure(
            count != 0 && found_entry,
            "function requires an entry block",
        )?;
        out.resize(count as usize, A::INVALID_BLOCK_REF)?;
        stack.clear();
        stack.push(adaptor.cur_entry_block())?;
        let mut index = count;
        while let Some(&block) = stack.last() {
            let info = adaptor.block_info2(block);
            if info & 8 != 0 {
                stack.pop();
                continue;
            }
            if info & 4 != 0 {
                stack.pop();
                index = index
                    .checked_sub(1)
                    .ok_or_else(|| invalid("successor outside current function"))?;
                adaptor.block_set_info(block, index);
                adaptor.block_set_info2(block, info | 8);
                out[index as usize] = block;
                continue;
            }
            adaptor.block_set_info2(block, info | 4);
            let start = stack.len();
            for successor in adaptor.block_succs(block) {
                ensure(
                    successor != adaptor.cur_entry_block(),
                    "entry block has an incoming edge",
                )?;
                let info = adaptor.block_info2(successor);
                if info & 3 == 1 {
                    adaptor.block_set_info2(successor, (info & !3) | 2);
                } else if info & 3 == 0 {
                    adaptor.block_set_info2(successor, info | 1);
                }
                if adaptor.block_info2(successor) & 12 == 0 {
                    stack.push(successor)?;
                }
            }
            if stack.len() - start == 2 {
                if adaptor.block_info(stack[start]) > adaptor.block_info(stack[start + 1]) {
                    stack.swap(start, start + 1);
                }
            } else if stack.len() - start > 2 {
                stack[start..].sort_unstable_by_key(|block| adaptor.block_info(*block));
            }
        }
        if index != 0 {
            out.erase(0..index as usize)?;
            for (idx, block) in out.iter().enumerate() {
                adaptor.block_set_info(*block, idx as u32);
            }
            if cfg!(debug_assertions) {
                for block in adaptor.cur_blocks() {
                    if adaptor.block_info2(block) == 0 {
                        adaptor.block_set_info(block, u32::MAX);
                    }
                }
            }
        }
        if tpde_base::LOGGING {
            tpde_base::tpde_log_trace!("Finished building RPO for blocks:");
            for (_idx, _block) in out.iter().enumerate() {
                tpde_base::tpde_log_trace!("Index {}: {}", _idx, adaptor.block_fmt_ref(*_block));
            }
        }
        Ok(())
    }

    fn tag_loop_head(infos: &mut [BlockInfo], block: u32, head: u32) {
        if block == head || head == 0 {
            return;
        }
        let (mut cur1, mut cur2) = (block, head);
        while infos[cur1 as usize].iloop_header != 0 {
            let ih = infos[cur1 as usize].iloop_header;
            if ih == cur2 {
                return;
            }
            if infos[ih as usize].dfsp_pos < infos[cur2 as usize].dfsp_pos {
                infos[cur1 as usize].iloop_header = cur2;
                cur1 = cur2;
                cur2 = ih;
            } else {
                cur1 = ih;
            }
        }
        infos[cur1 as usize].iloop_header = cur2;
    }

    fn identify_loops(&mut self, adaptor: &A) -> PgResult<()> {
        let count = self.block_rpo.len();
        self.loop_parent.clear();
        self.loop_parent.resize(count, 0)?;
        self.loop_heads.clear();
        self.loop_heads.resize(count as u32)?;
        self.block_infos.clear();
        self.block_infos.resize_default(count)?;
        let mut stack: SmallVectorGuard<'m, Traversal<_>, SMALL_BLOCK_NUM> =
            SmallVectorGuard::new(self.mcx);
        self.block_infos[0].traversed = true;
        self.block_infos[0].dfsp_pos = 1;
        stack.push(Traversal {
            block_idx: 0,
            dfsp_pos: 1,
            successors: adaptor.block_succs(self.block_rpo[0]).into_iter(),
        })?;
        while let Some(state) = stack.last_mut() {
            let block_idx = state.block_idx;
            if let Some(successor) = state.successors.next() {
                let successor = adaptor.block_info(successor);
                ensure(
                    (successor as usize) < count,
                    "successor outside reachable blocks",
                )?;
                if successor == block_idx {
                    self.block_infos[block_idx as usize].self_loop = true;
                }
                let info = self.block_infos[successor as usize];
                if info.traversed {
                    if info.dfsp_pos > 0 {
                        Self::tag_loop_head(&mut self.block_infos, block_idx, successor);
                    } else if info.iloop_header != 0 {
                        let mut head = info.iloop_header;
                        if self.block_infos[head as usize].dfsp_pos > 0 {
                            Self::tag_loop_head(&mut self.block_infos, block_idx, head);
                        } else {
                            while self.block_infos[head as usize].iloop_header != 0 {
                                head = self.block_infos[head as usize].iloop_header;
                                if self.block_infos[head as usize].dfsp_pos > 0 {
                                    Self::tag_loop_head(&mut self.block_infos, block_idx, head);
                                    break;
                                }
                            }
                        }
                    }
                } else {
                    let depth = state
                        .dfsp_pos
                        .checked_add(1)
                        .ok_or_else(|| invalid("CFG depth overflow"))?;
                    self.block_infos[successor as usize].traversed = true;
                    self.block_infos[successor as usize].dfsp_pos = depth;
                    stack.push(Traversal {
                        block_idx: successor,
                        dfsp_pos: depth,
                        successors: adaptor
                            .block_succs(self.block_rpo[successor as usize])
                            .into_iter(),
                    })?;
                }
            } else {
                self.block_infos[block_idx as usize].dfsp_pos = 0;
                stack.pop();
                if let Some(parent) = stack.last() {
                    let head = self.block_infos[block_idx as usize].iloop_header;
                    Self::tag_loop_head(&mut self.block_infos, parent.block_idx, head);
                }
            }
        }
        for (idx, info) in self.block_infos.iter().enumerate() {
            if info.iloop_header != 0 {
                self.loop_parent[idx] = info.iloop_header;
                self.loop_heads.mark_set(info.iloop_header)?;
            }
            if info.self_loop {
                self.loop_heads.mark_set(idx as u32)?;
            }
        }
        Ok(())
    }

    fn build_loop_tree_and_block_layout(&mut self, adaptor: &A) -> PgResult<()> {
        let count = self.block_rpo.len();
        self.block_loops.clear();
        self.block_loops.resize(count, u32::MAX)?;
        self.loops.clear();
        self.loops.push(Loop::new(0, 0))?;
        self.loops[0].num_blocks = 1;
        self.block_loops[0] = 0;
        for idx in 1..count {
            let parent = self.build_or_get_parent_loop(idx as u32)?;
            if self.loop_heads.is_set(idx as u32)? {
                let mut loop_idx = self.block_loops[idx];
                if loop_idx == u32::MAX {
                    loop_idx = self.loops.len() as u32;
                    self.loops
                        .push(Loop::new(self.loops[parent as usize].level + 1, parent))?;
                    self.block_loops[idx] = loop_idx;
                }
                self.loops[loop_idx as usize].num_blocks += 1;
            } else {
                self.block_loops[idx] = parent;
                self.loops[parent as usize].num_blocks += 1;
            }
        }
        for idx in (1..self.loops.len()).rev() {
            let info = self.loops[idx];
            self.loops[info.parent as usize].num_blocks += info.num_blocks;
        }
        tpde_assert_eq!(self.loops[0].num_blocks as usize, count);
        self.block_layout.resize(count, A::INVALID_BLOCK_REF)?;
        self.block_loop_map.resize(count, 0)?;
        self.loops[0].begin = BlockIndex(0);
        self.loops[0].end = BlockIndex(0);
        self.sunk.clear();
        for idx in 0..count {
            let loop_idx = self.block_loops[idx];
            // D3651: a cold detour inside a loop (never a header) takes its place after every other block: the end of
            // its innermost loop's range, so the hot path around it is straight-line again.
            if loop_idx != 0 && !self.loop_heads.is_set(idx as u32)? && self.may_sink(adaptor, self.block_rpo[idx]) {
                self.sunk.push(idx as u32)?;
                continue;
            }
            self.place_block(adaptor, idx, loop_idx)?;
        }
        for i in 0..self.sunk.len() {
            let idx = self.sunk[i] as usize;
            self.sunk[i] = self.place_block(adaptor, idx, self.block_loops[idx])?;
        }
        tpde_assert_eq!(self.loops[0].end.0 as usize, count);
        Ok(())
    }

    /// The next layout index of `loop_idx`'s range to the RPO block `idx`.
    fn place_block(&mut self, adaptor: &A, idx: usize, loop_idx: u32) -> PgResult<u32> {
        if self.loops[loop_idx as usize].begin == INVALID_BLOCK_IDX {
            self.layout_loop(loop_idx)?;
        }
        let block = self.block_rpo[idx];
        let block_idx = self.loops[loop_idx as usize].end.0;
        self.loops[loop_idx as usize].end.0 += 1;
        self.block_layout[block_idx as usize] = block;
        self.block_loop_map[block_idx as usize] = loop_idx;
        adaptor.block_set_info(block, block_idx);
        Ok(block_idx)
    }

    /// The adaptor asks to sink `block` (`IrAdaptor::block_sink`) and no successor of it has phis (an edge into a block
    /// laid out before its source moves no phi values here).
    fn may_sink(&self, adaptor: &A, block: A::BlockRef) -> bool {
        adaptor.block_sink(block)
            && adaptor
                .block_succs(block)
                .into_iter()
                .all(|succ| adaptor.block_phis(succ).into_iter().next().is_none())
    }

    /// D3651: a sunk block left the RPO position the live ranges stand for. A value live into one of its successors
    /// inside its loop lives through it too: its range must reach the sunk block, to its end (`last_full`), or the
    /// detour's code could take the value's register or slot before the successor reads it. A value the sunk block
    /// defines must be read in it alone (it now sits after the blocks that would read it).
    fn extend_through_sunk(&mut self, adaptor: &A) -> PgResult<()> {
        self.sunk_edges.clear();
        for i in 0..self.sunk.len() {
            let c = self.sunk[i];
            let block = self.block_layout[c as usize];
            for inst in adaptor.block_insts(block) {
                for result in adaptor.inst_results(inst) {
                    if adaptor.val_ignore_in_liveness_analysis(result) {
                        continue;
                    }
                    let Some(info) = self.liveness.get(adaptor.val_local_idx(result).get() as usize) else { continue };
                    ensure(
                        info.epoch != self.liveness_epoch || (info.first.0 == c && info.last.0 == c),
                        "a sunk block's value is read outside it",
                    )?;
                }
            }
            let lp = self.loops[self.block_loop_map[c as usize] as usize];
            for succ in adaptor.block_succs(block) {
                let s = adaptor.block_info(succ);
                if s != c && lp.begin.0 <= s && s < lp.end.0 {
                    self.sunk_edges.push((s, c))?;
                }
            }
        }
        let k = self.sunk_edges.len();
        if k == 0 {
            return Ok(());
        }
        self.sunk_edges.sort_unstable();
        // Sparse table: row r holds, per i, the largest sunk index among edges i .. i + 2^r.
        self.sunk_reach.clear();
        for i in 0..k {
            let c = self.sunk_edges[i].1;
            self.sunk_reach.push(c)?;
        }
        let mut width = 1usize;
        let mut row = 0usize;
        while 2 * width <= k {
            for i in 0..k {
                let a = self.sunk_reach[row * k + i];
                let b = if i + width < k { self.sunk_reach[row * k + i + width] } else { a };
                self.sunk_reach.push(a.max(b))?;
            }
            row += 1;
            width *= 2;
        }
        let edges = &self.sunk_edges;
        let reach = &self.sunk_reach;
        let farthest = edges.iter().map(|e| e.1).max().unwrap_or(0);
        for info in self.liveness.iter_mut() {
            if info.epoch != self.liveness_epoch || info.last.0 > farthest {
                continue;
            }
            // The edges whose successor the range covers past its first block: first < s <= last.
            let lo = edges.partition_point(|e| e.0 <= info.first.0);
            let hi = edges.partition_point(|e| e.0 <= info.last.0);
            if lo >= hi {
                continue;
            }
            let r = (usize::BITS - 1 - (hi - lo).leading_zeros()) as usize;
            let c = reach[r * k + lo].max(reach[r * k + hi - (1 << r)]);
            // `>=`: a value the sunk block reads (its range ends there) that also lives into a successor must outlast the
            // block, not die at its read in it.
            if c >= info.last.0 {
                info.last.0 = c;
                info.last_full = true;
            }
        }
        Ok(())
    }

    fn build_or_get_parent_loop(&mut self, mut block: u32) -> PgResult<u32> {
        self.chain.clear();
        let mut parent_loop;
        loop {
            block = self.loop_parent[block as usize];
            parent_loop = self.block_loops[block as usize];
            if parent_loop != u32::MAX {
                break;
            }
            ensure(
                self.chain.len() < self.block_rpo.len(),
                "cyclic loop parent map",
            )?;
            self.chain.push(block)?;
        }
        while let Some(block) = self.chain.pop() {
            let idx = self.loops.len() as u32;
            self.loops.push(Loop::new(
                self.loops[parent_loop as usize].level + 1,
                parent_loop,
            ))?;
            self.block_loops[block as usize] = idx;
            parent_loop = idx;
        }
        Ok(parent_loop)
    }

    fn layout_loop(&mut self, mut idx: u32) -> PgResult<()> {
        self.chain.clear();
        while self.loops[idx as usize].begin == INVALID_BLOCK_IDX {
            self.chain.push(idx)?;
            idx = self.loops[idx as usize].parent;
        }
        while let Some(idx) = self.chain.pop() {
            let info = self.loops[idx as usize];
            let parent = &mut self.loops[info.parent as usize];
            let begin = parent.end;
            parent.end.0 += info.num_blocks;
            tpde_assert!(parent.end.0 - parent.begin.0 <= parent.num_blocks);
            self.loops[idx as usize].begin = begin;
            self.loops[idx as usize].end = begin;
        }
        Ok(())
    }

    fn liveness_maybe(&mut self, adaptor: &A, value: A::ValueRef) -> PgResult<usize> {
        let idx = adaptor.val_local_idx(value).get();
        if A::HighestValue::PROVIDED {
            ensure(
                (idx as usize) < self.liveness.len(),
                "value exceeds declared highest index",
            )?;
        } else if self.liveness_max_value <= idx {
            self.liveness_max_value = idx;
            if self.liveness.len() <= idx as usize {
                let len = idx
                    .checked_add(0x100)
                    .ok_or_else(|| invalid("liveness index overflow"))?;
                self.liveness.resize_default(len as usize)?;
            }
        }
        Ok(idx as usize)
    }

    fn visit(&mut self, adaptor: &A, value: A::ValueRef, block_idx: u32) -> PgResult<()> {
        tpde_base::tpde_log_trace!(
            "  Visiting value {} in block {}",
            adaptor.value_fmt_ref(value),
            block_idx
        );
        if adaptor.val_ignore_in_liveness_analysis(value) {
            tpde_base::tpde_log_trace!("    value is ignored");
            return Ok(());
        }
        let idx = self.liveness_maybe(adaptor, value)?;
        let block_loop_idx = self.block_loop_map[block_idx as usize];
        let visit_weight = 1u32 << (2 * self.loops[block_loop_idx as usize].level).min(20);
        let info = &mut self.liveness[idx];
        if info.epoch != self.liveness_epoch {
            tpde_base::tpde_log_trace!("    initializing liveness info, lcl is {}", block_loop_idx);
            *info = LivenessInfo {
                first: BlockIndex(block_idx),
                last: BlockIndex(block_idx),
                ref_count: 1,
                lowest_common_loop: block_loop_idx,
                last_full: false,
                epoch: self.liveness_epoch,
                weight: visit_weight,
                crosses_call: false,
                crosses_cold_call: false,
            };
            return Ok(());
        }
        info.ref_count = info
            .ref_count
            .checked_add(1)
            .ok_or_else(|| invalid("liveness reference count overflow"))?;
        info.weight = info.weight.saturating_add(visit_weight);
        tpde_base::tpde_log_trace!("    increasing ref_count to {}", info.ref_count);
        if info.lowest_common_loop == block_loop_idx {
            tpde_base::tpde_log_trace!("    lcl is same as block loop");
            info.extend_block(block_idx, false);
            tpde_base::tpde_log_trace!(
                "    new interval {}->{}, lf: {}",
                info.first.0,
                info.last.0,
                info.last_full
            );
            return Ok(());
        }
        let live_loop = self.loops[info.lowest_common_loop as usize];
        let block_loop = self.loops[block_loop_idx as usize];
        if live_loop.level < block_loop.level && block_loop.begin.0 < live_loop.end.0 {
            tpde_assert!(block_loop.end.0 <= live_loop.end.0);
            tpde_base::tpde_log_trace!("    block_loop {} is nested inside lcl", block_loop_idx);
            let target_level = live_loop.level + 1;
            let mut current = block_loop_idx;
            while self.loops[current as usize].level != target_level {
                current = self.loops[current as usize].parent;
            }
            tpde_base::tpde_log_trace!("    target_loop is {}", current);
            info.extend_loop(self.loops[current as usize], false);
            tpde_base::tpde_log_trace!(
                "    new interval {}->{}, lf: {}",
                info.first.0,
                info.last.0,
                info.last_full
            );
            return Ok(());
        }
        tpde_base::tpde_log_trace!(
            "    block_loop {} is nested higher or in a different loop",
            block_loop_idx
        );
        let (mut lhs, mut rhs) = (info.lowest_common_loop, block_loop_idx);
        let (mut prev_lhs, mut prev_rhs) = (lhs, rhs);
        while lhs != rhs {
            let lhs_level = self.loops[lhs as usize].level;
            let rhs_level = self.loops[rhs as usize].level;
            if lhs_level >= rhs_level {
                prev_lhs = lhs;
                lhs = self.loops[lhs as usize].parent;
            }
            if lhs_level <= rhs_level {
                prev_rhs = rhs;
                rhs = self.loops[rhs as usize].parent;
            }
        }
        tpde_assert!(self.loops[lhs as usize].begin.0 <= live_loop.begin.0);
        tpde_assert!(self.loops[lhs as usize].end.0 >= live_loop.end.0);
        tpde_base::tpde_log_trace!("    new lcl is {}", lhs);
        info.lowest_common_loop = lhs;
        tpde_assert_eq!(self.loops[prev_lhs as usize].parent, lhs);
        info.extend_loop(self.loops[prev_lhs as usize], false);
        if lhs == block_loop_idx {
            info.extend_block(block_idx, true);
        } else {
            tpde_assert_eq!(self.loops[prev_rhs as usize].parent, lhs);
            info.extend_loop(self.loops[prev_rhs as usize], true);
        }
        tpde_base::tpde_log_trace!(
            "    new interval {}->{}, lf: {}",
            info.first.0,
            info.last.0,
            info.last_full
        );
        Ok(())
    }

    fn compute_liveness(&mut self, adaptor: &A) -> PgResult<()> {
        tpde_base::tpde_log_trace!("Starting Liveness Analysis");
        self.liveness_epoch = self.liveness_epoch.wrapping_add(1);
        if self.liveness_epoch == 0 {
            self.liveness.clear();
            self.liveness_epoch = 1;
        }
        if let Some(highest) = A::HighestValue::highest_value_index(adaptor) {
            self.liveness_max_value = highest;
            if highest as usize >= self.liveness.len() {
                let len = highest
                    .checked_add(1)
                    .ok_or_else(|| invalid("highest value index overflow"))?;
                self.liveness.resize_default(len as usize)?;
            }
        } else {
            self.liveness_max_value = 0;
        }
        self.num_insts = 0;
        tpde_assert!(self.block_layout[0] == adaptor.cur_entry_block());
        if A::TPDE_LIVENESS_VISIT_ARGS {
            for arg in adaptor.cur_args() {
                self.visit(adaptor, arg, 0)?;
            }
        }
        for block_idx in 0..self.block_layout.len() {
            let block = self.block_layout[block_idx];
            let block_loop = self.block_loop_map[block_idx] as usize;
            tpde_base::tpde_log_trace!(
                "Analyzing block {} ('{}')",
                block_idx,
                adaptor.block_fmt_ref(block)
            );
            let mut has_phis = false;
            for phi in adaptor.block_phis(block) {
                tpde_base::tpde_log_trace!("Analyzing phi {}", adaptor.value_fmt_ref(phi));
                has_phis = true;
                let phi_ref = adaptor.val_as_phi(phi);
                for slot in 0..phi_ref.incoming_count() {
                    let incoming_block = phi_ref.incoming_block_for_slot(slot);
                    let incoming_value = phi_ref.incoming_val_for_slot(slot);
                    if adaptor.block_info2(incoming_block) == 0 {
                        tpde_base::tpde_log_trace!(
                            "ignoring phi input from unreachable pred ({})",
                            adaptor.block_fmt_ref(incoming_block)
                        );
                        continue;
                    }
                    let incoming_idx = adaptor.block_info(incoming_block);
                    self.visit(adaptor, incoming_value, incoming_idx)?;
                    self.visit(adaptor, phi, incoming_idx)?;
                }
            }
            if has_phis {
                adaptor.block_set_info2(block, adaptor.block_info2(block) | 16);
            }
            for inst in adaptor.block_insts(block) {
                tpde_base::tpde_log_trace!("Analyzing instruction {}", adaptor.inst_fmt_ref(inst));
                for result in adaptor.inst_results(inst) {
                    self.visit(adaptor, result, block_idx as u32)?;
                    self.loops[block_loop].definitions = self.loops[block_loop]
                        .definitions
                        .checked_add(1)
                        .ok_or_else(|| invalid("loop definition count overflow"))?;
                }
                for operand in adaptor.inst_operands(inst) {
                    self.visit(adaptor, operand, block_idx as u32)?;
                }
                self.num_insts = self
                    .num_insts
                    .checked_add(1)
                    .ok_or_else(|| invalid("instruction count overflow"))?;
            }
        }
        if !self.sunk.is_empty() {
            self.extend_through_sunk(adaptor)?;
        }
        self.calls_before.clear();
        self.calls_before.resize(self.block_layout.len() + 1, 0)?;
        self.cold_calls_before.clear();
        self.cold_calls_before
            .resize(self.block_layout.len() + 1, 0)?;
        for block_idx in 0..self.block_layout.len() {
            let calls = adaptor.block_calls(self.block_layout[block_idx]);
            self.calls_before[block_idx + 1] = self.calls_before[block_idx] + calls.hot as u32;
            self.cold_calls_before[block_idx + 1] =
                self.cold_calls_before[block_idx] + calls.cold as u32;
        }
        for info in self.liveness.iter_mut() {
            if info.epoch == self.liveness_epoch {
                let (first, last) = (info.first.0 as usize, info.last.0 as usize);
                info.crosses_call =
                    first <= last && self.calls_before[last + 1] > self.calls_before[first];
                info.crosses_cold_call = first <= last
                    && self.cold_calls_before[last + 1] > self.cold_calls_before[first];
            }
        }
        for idx in (1..self.loops.len()).rev() {
            let info = self.loops[idx];
            let definitions = info
                .definitions_in_childs
                .checked_add(info.definitions)
                .ok_or_else(|| invalid("nested definition count overflow"))?;
            let parent = &mut self.loops[info.parent as usize];
            parent.definitions_in_childs = parent
                .definitions_in_childs
                .checked_add(definitions)
                .ok_or_else(|| invalid("nested definition count overflow"))?;
        }
        if cfg!(debug_assertions) {
            for info in self.liveness.iter_mut() {
                if info.ref_count == u32::MAX {
                    info.ref_count = 0;
                }
            }
        }
        tpde_base::tpde_log_trace!("Finished Liveness Analysis");
        Ok(())
    }
}

#[cfg(test)]
#[path = "../tests/analyzer_unit.rs"]
mod tests;
