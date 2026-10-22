//! setop.setop's skip form (R9-L09, a #[path] submodule of m9.rs, the m8.rs -> m8_spool.rs pattern; F-ZERO split it out,
//! V4R-413). Owner M9-HOSTS. Budget 100 lines (files-extra.tsv, +25 % gate).
//!
//! lowerv4 setop/empty_left.rs declares it on the left group's producer, naming the right group's producer as its peer:
//! after the left producer ran, an empty left (no record; a region never laid out; a plain left counting no row) runs
//! the right side's pipelines as if no claim reached them (`Exec::run_empty`), as PG never reads the inner plan then.
//! Those pipelines come from the program (`skip_mask`), never from `arg` (D1274), so a program a pass reorders keeps them.
use gravity_driver::Exec;
use memledger::ChargedVec;
use gravity_program::protos::Proto;
use gravity_program::Program;

use super::super::Live;
use crate::ops::setop::proto::{plain_count, MIN_REGION};

/// setop/empty_left.rs's skip `arg`: bits 48..64 the plain left record's sentinel offset + 1, 0 over an owned table.
const SKIP_PLAIN_SHIFT: u32 = 48;

struct SkipLive {
    pipeline: usize,
    slot: u8,
    /// The right side's pipelines (word w bit b = pipeline 64 w + b; D4050: past pipeline 63 too).
    mask: ChargedVec<u64>,
    /// A plain left record's sentinel byte offset (F-ZERO); `None` over an owned table.
    plain: Option<usize>,
}

/// The skip form's live state for proto `p` (it rides the left producer, `pipeline`, and names the right producer,
/// `peer`; the emit is the pipeline that reads the left slot from it), `None` when nothing is skippable.
pub(crate) fn prepare(p: &Proto, program: &Program, mask: ChargedVec<u64>) -> Option<Box<dyn Live>> {
    let (k, peer) = (p.pipeline as usize, p.peer as usize);
    let emit = (k + 1..program.pipelines.len()).find(|&c| producer_of(program, c, p.slot) == Some(k))?;
    let mask = skip_mask(program, emit, k, peer, mask);
    let plain = ((p.arg >> SKIP_PLAIN_SHIFT) as usize).checked_sub(1);
    mask.iter().any(|&w| w != 0).then(|| Box::new(SkipLive { pipeline: k, slot: p.slot, mask, plain }) as Box<dyn Live>) // alloc: Exec
}

/// The pipeline slot `slot` read by pipeline `c` comes from: its `consumes_from` life, else the nearest earlier producer.
pub(crate) fn producer_of(program: &Program, c: usize, slot: u8) -> Option<usize> {
    let p = program.pipelines.get(c)?;
    let i = p.consumes.iter().position(|&s| s == slot)?;
    match p.consumes_from.get(i) {
        Some(l) => Some(l.0 as usize),
        None => (0..c).rev().find(|&q| program.pipelines[q].produces() == Some(slot)),
    }
}

/// The right side's pipelines an empty left runs empty (D1274: from the program, in program order): every pipeline after
/// the left producer `lp` up to the right producer `rp` that no pipeline outside the side but the emit `k` reads.
fn skip_mask(program: &Program, k: usize, lp: usize, rp: usize, mask: ChargedVec<u64>) -> ChargedVec<u64> {
    let reads = |c: usize, x: usize| program.pipelines[c].consumes.iter().any(|&s| producer_of(program, c, s) == Some(x));
    mask_of(program.pipelines.len(), &reads, k, lp, rp, mask)
}

/// `skip_mask` over `n` pipelines where `reads(c, x)` says pipeline `c` reads pipeline `x`'s product (word w bit b =
/// pipeline 64 w + b). D4050: every pipeline of the side, past the 63rd too (a left of 70 UNION ALL arms puts the right
/// side there), sized here at the prepare, never on the run.
fn mask_of(n: usize, reads: &dyn Fn(usize, usize) -> bool, k: usize, lp: usize, rp: usize, mut mask: ChargedVec<u64>) -> ChargedVec<u64> {
    let side = |q: usize| q > lp && q <= rp;
    mask.with_vec(|v| v.resize(rp / 64 + 1, 0)); // alloc: Exec charged mask.owner()
    for x in (lp + 1..=rp).filter(|&x| (0..n).all(|c| side(c) || c == k || !reads(c, x))) {
        mask[x / 64] |= 1 << (x % 64);
    }
    mask
}

impl Live for SkipLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::AFTER
    }

    /// An empty left group (no record; never laid out; a plain left counting no row) runs the masked right side empty.
    fn after_pipeline(&self, pipeline: usize, exec: &mut Exec) -> Option<gravity_driver::Result<()>> {
        if pipeline != self.pipeline {
            return None;
        }
        let empty = match self.plain {
            Some(sent) => plain_count(exec, self.slot, sent) == Some(0),
            None => {
                let r = exec.node_mut().slot_region(self.slot);
                r.len() < MIN_REGION || super::raw::owned_empty(r)
            }
        };
        if empty {
            exec.run_empty_words(&self.mask);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::mask_of;
    use memledger::ChargedVec;

    /// D1274: the skip set is the right side (lp, rp], less a pipeline another pipeline outside the side reads; the
    /// emit's reads do not count.
    #[test]
    fn the_skip_set_is_the_right_side_nothing_else_reads() {
        // 0 left producer, 1 a build the right side reads, 2 the right producer, 3 the emit, 4 a later reader of 1
        let reads = |c: usize, x: usize| matches!((c, x), (2, 1) | (3, 0) | (3, 2) | (4, 1));
        assert_eq!(*mask_of(5, &reads, 3, 0, 2, ChargedVec::none()), [0b100]);
        let alone = |c: usize, x: usize| matches!((c, x), (2, 1) | (3, 0) | (3, 2));
        assert_eq!(*mask_of(4, &alone, 3, 0, 2, ChargedVec::none()), [0b110]);
    }

    /// D4050: a right side past pipeline 63 (a left of 70 UNION ALL arms: 69 the left producer, 70 a build the right
    /// side reads, 71 the right producer, 72 the emit) is in the set, in its word.
    #[test]
    fn the_skip_set_reaches_past_pipeline_63() {
        let reads = |c: usize, x: usize| matches!((c, x), (71, 70) | (72, 69) | (72, 71));
        assert_eq!(*mask_of(73, &reads, 72, 69, 71, ChargedVec::none()), [0, 1 << 6 | 1 << 7]);
        let straddle = |c: usize, x: usize| matches!((c, x), (64, 62) | (65, 61) | (65, 64));
        assert_eq!(*mask_of(66, &straddle, 65, 61, 64, ChargedVec::none()), [1 << 62 | 1 << 63, 1]);
    }
}
