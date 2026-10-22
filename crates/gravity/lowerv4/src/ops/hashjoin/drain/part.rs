//! The drain source's phase 2 (split out of join/drain_src.rs for its budget): past
//! the resident records of a RIGHT / FULL / RIGHT SEMI / RIGHT ANTI build that spilled, each spilled partition in turn
//! (its drain part, else its build part), every record wanted. Three frame words carry it across windows and resumes: the
//! phase (`PH_RES` the resident records, `p + 1` spilled partition `p` with its next chunk's byte above `PH_PART` bits
//! (D2715: a part past a region's length loads in chunks), `PH_END`), the partition's region and its record count. D3024
//! (b): a wide drain's claim takes the spilled partitions whose slice falls in it (`claim_parts`). A helper file (no
//! rows). Owner M7-DRAIN (M1 scope). Budget 130 lines.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};

use crate::face::kinds::Face;
use crate::frame::env::Env;
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Px, WordKey};

use crate::ops::hashjoin::spill::build::raise_neg;
use crate::ops::hashjoin::drain::source::{min, span};
use crate::ops::hashjoin::drain::raw::drain_part;
use crate::ops::hashjoin::drain::spill::stream_of;

/// The drain's phases (its first frame word): the resident records, then spilled partition `p` as `p + 1`, then none.
pub(crate) const PH_RES: i64 = 0;
pub(crate) const PH_END: i64 = 1 << 62;
/// The phase's bits naming the partition (`p + 1`); the next chunk's byte sits above them.
const PH_PART: u32 = 8;

/// D3024 (b): `drain_part`'s `how` word: bit 0 RIGHT SEMI's marks; `CLAIMED` set when slices `[lo, hi)` of
/// `OWNER_RUN_SLICES` (bits 16.. and 32..) bound the partitions it walks (`claim_parts`), clear for every partition.
pub(crate) const CLAIMED: u64 = 1 << 8;

/// D3024 (b): the spilled partitions `[a, b)` of `2^bits` that slices `[lo, hi)` of `OWNER_RUN_SLICES` (S) take, each
/// partition `p` belonging to slice `floor(p * S / 2^bits)` (`bits` <= 6: `SPILLED` is one word): the partitions of any set
/// of claims that cover `[0, S)` once are each taken once. Slices past S clamp to S (a section end's claim: none).
pub(crate) fn claim_parts(lo: u64, hi: u64, bits: u32) -> (u32, u32) {
    let s = gravity_abi::breaker::OWNER_RUN_SLICES;
    let at = |x: u64| (x.min(s) << bits).div_ceil(s) as u32;
    (at(lo), at(hi))
}

/// D2715: the phase of spilled partition `p` (`p < 64`: `SPILLED` is one word) sent up to its next chunk at byte `next`
/// (0: its last chunk): `p + 1`, the byte above `PH_PART` bits. Never `PH_RES` nor `PH_END` (`next` < 2^54).
pub(crate) fn phase(p: u32, next: usize) -> i64 {
    (p as i64 + 1) | (next as i64) << PH_PART
}

/// The partition a phase resumes at and its first byte: the named one at its next chunk, else the one after it from 0
/// (`PH_RES`: partition 0).
pub(crate) fn resume(ph: u64) -> (u32, usize) {
    match ((ph & ((1 << PH_PART) - 1)) as u32, (ph >> PH_PART) as usize) {
        (part, 0) => (part, 0),
        (part, next) => (part - 1, next),
    }
}

/// Phase 2 of drain pipeline `k` over the build it consumes as region `consumed`: its three frame words (byte offsets)
/// and the build slot's stream.
pub(crate) type Spill = ([i32; 3], u64);

pub(crate) fn words(px: &mut Px<'_, '_>, k: usize, consumed: usize) -> Spill {
    let mut w = |i: u8| px.frame_word(WordKey::Open { face: Face::Join, k, j: u64::MAX, i }) as i32 * 8;
    ([w(0), w(1), w(2)], stream_of(consumed))
}

/// The words, zero (the resident phase, no region) at a fresh open.
pub(crate) fn open_words((ws, _): Spill, w: &mut OpenWords) {
    for (i, what) in ["drain phase", "drain region", "drain records"].into_iter().enumerate() {
        w.declare(OpenWord::open(what, Base::Words, ws[i], Type::I64, 0));
    }
}

/// The window's entry: the resident phase goes on to `l_res`; past the end, `l_eof`; in partition `p`, its window
/// `[pos, min(pos + span(), n))` over the partition's region to `l_win` (cursor, end, region, records, marks: none, every
/// record wanted), or `l_next` past its last record.
pub(crate) fn entry(b: &mut FunctionBuilder<'_>, env: &mut Env, (w, _): Spill, base: usize, l_res: Block, l_eof: Block, l_next: Block, l_win: Block) {
    let ph = env.load_word(b, w[0]);
    let res = b.iconst(Type::I64, PH_RES);
    let at_res = b.icmp(IntCond::Eq, ph, res);
    let l_part = b.create_block(&[]);
    b.cond_br(at_res, (l_res, &[]), (l_part, &[]));
    b.switch_to(l_part);
    let end = b.iconst(Type::I64, PH_END);
    let past = b.icmp(IntCond::Eq, ph, end);
    let l_in = b.create_block(&[]);
    b.cond_br(past, (l_eof, &[env.zero]), (l_in, &[]));
    b.switch_to(l_in);
    let (reg, n) = (env.load_word(b, w[1]), env.load_word(b, w[2]));
    let pos = crate::frame::resume::resume_pos(b, env);
    let done = b.icmp(IntCond::Sge, pos, n);
    let l_go = b.create_block(&[]);
    b.cond_br(done, (l_next, &[]), (l_go, &[]));
    b.switch_to(l_go);
    let span = b.iconst(Type::I64, span());
    let far = b.int_bin(IntBinOp::Add, pos, span);
    let e = min(b, far, n);
    let hdr = b.iconst(Type::I64, base as i64);
    let recs = b.int_bin(IntBinOp::Add, reg, hdr);
    let rp = b.conv(ConvOp::IntToPtr, Type::Ptr, reg);
    b.br(l_win, &[pos, e, rp, recs, rp]);
}

/// `l_next`, past a phase's last record: the next spilled partition with a record to send (an empty window at 0 into
/// `l_win`: the frame's window loop comes back to `entry` at its first record, join/spill_pair.rs's pattern), or `l_eof`.
/// `r` is the resident region, `marked` RIGHT SEMI's, `claim` a wide drain's slices (clamped to S: `claim_parts`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn next(b: &mut FunctionBuilder<'_>, env: &mut Env, (w, stream): Spill, base: usize, r: Value, marked: bool, claim: Option<(Value, Value)>, l_next: Block, l_eof: Block, l_win: Block) {
    b.switch_to(l_next);
    let f = b.declare_host_once("m7_drain_part", &[Type::Ptr, Type::I64, Type::Ptr, Type::I64, Type::I64, Type::I64, Type::Ptr], Some(Type::I64), drain_part as *const () as usize, HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() });
    let out = b.declare_stack_slot(16, 8);
    let out = b.stack_addr(out);
    let (ph, reg) = (env.load_word(b, w[0]), env.load_word(b, w[1]));
    let st = b.iconst(Type::I64, stream as i64);
    let mut how = b.iconst(Type::I64, marked as i64 | if claim.is_some() { CLAIMED as i64 } else { 0 });
    if let Some((lo, hi)) = claim {
        let (s16, s32) = (b.iconst(Type::I64, 16), b.iconst(Type::I64, 32));
        let (lo, hi) = (b.int_bin(IntBinOp::Shl, lo, s16), b.int_bin(IntBinOp::Shl, hi, s32));
        let both = b.int_bin(IntBinOp::Or, lo, hi);
        how = b.int_bin(IntBinOp::Or, how, both);
    }
    let got = b.call_host(f, &[env.ctx, st, r, ph, reg, how, out], Some(Type::I64)).expect("m7_drain_part returns its phase");
    raise_neg(b, env, got);
    env.store_word(b, w[0], got);
    let (nr, nn) = (b.load(Type::I64, out, 0), b.load(Type::I64, out, 8));
    env.store_word(b, w[1], nr);
    env.store_word(b, w[2], nn);
    let end = b.iconst(Type::I64, PH_END);
    let past = b.icmp(IntCond::Eq, got, end);
    let l_open = b.create_block(&[]);
    b.cond_br(past, (l_eof, &[env.zero]), (l_open, &[]));
    b.switch_to(l_open);
    let rp = b.conv(ConvOp::IntToPtr, Type::Ptr, nr);
    let hdr = b.iconst(Type::I64, base as i64);
    let recs = b.int_bin(IntBinOp::Add, nr, hdr);
    b.br(l_win, &[env.zero, env.zero, rp, recs, rp]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D2715: the phase word round-trips a chunked partition at its boundaries (the first chunk, a later chunk, the last
    /// chunk, a part that fits whole, the last partition, the largest chunk byte), and never reads as `PH_RES` or `PH_END`
    /// or aliases the partition bits.
    #[test]
    fn a_drain_phase_carries_its_partition_and_next_chunk() {
        assert_eq!(resume(PH_RES as u64), (0, 0));
        let far = (1usize << (62 - PH_PART)) - 1;
        for p in [0u32, 1, 31, 63] {
            for next in [0usize, 8, 4096, 1 << 32, far] {
                let ph = phase(p, next);
                assert!(ph != PH_RES && ph != PH_END && ph > 0 && ph < PH_END, "p {p} next {next}");
                assert_eq!(ph & ((1 << PH_PART) - 1), p as i64 + 1);
                // a later chunk resumes the same partition at its byte; the last chunk (0) goes on to the next partition
                assert_eq!(resume(ph as u64), if next == 0 { (p + 1, 0) } else { (p, next) });
            }
        }
        // the last partition's last chunk resumes past every partition (the drain then ends)
        assert_eq!(resume(phase(63, 0) as u64), (64, 0));
    }

    /// D3024 (b): over every claim set a runner cuts (fixed slices of S, a few per claim, or the whole-range claim), the
    /// claims' records (abi `Owned::slice_runs`, the drain's `records_at`) tile `[0, T)` and their spilled partitions take
    /// each of `2^bits` once, for bits 0-6 and T at the slice edges; a section end's claim takes none.
    #[test]
    fn a_wide_drain_claim_set_takes_each_record_and_partition_once() {
        use gravity_abi::breaker::{Owned, OWNER_RUN_SLICES as S};
        let sets: Vec<Vec<(u64, u64)>> = [1u64, 2, 3, 5, 8, 11, 32, S]
            .iter()
            .map(|&per| (0..S.div_ceil(per)).map(|i| (i * per, ((i + 1) * per).min(S))).collect())
            .chain([vec![(0, u64::MAX)]])
            .collect();
        for set in &sets {
            for t in [0u64, 1, 63, 64, 65, 1_000_003, 1 << 40] {
                let mut at = 0;
                for &(lo, hi) in set {
                    let (a, z) = Owned::slice_runs(lo, hi, t);
                    assert_eq!(a, at, "set {set:?} T {t}: claim [{lo}, {hi}) starts where the last ended");
                    at = z;
                }
                assert_eq!(at, t, "set {set:?} T {t}: the claims end at T");
            }
            for bits in 0..=6u32 {
                let mut seen = vec![0u32; 1 << bits];
                for &(lo, hi) in set {
                    let (a, z) = claim_parts(lo, hi, bits);
                    for p in a..z {
                        seen[p as usize] += 1;
                        assert!((lo..hi.min(S)).contains(&((p as u64 * S) >> bits)), "set {set:?} bits {bits}: partition {p} outside its slice");
                    }
                }
                assert!(seen.iter().all(|&n| n == 1), "set {set:?} bits {bits}: {seen:?}");
            }
        }
        for bits in 0..=6u32 {
            let (a, z) = claim_parts(u64::MAX, u64::MAX, bits);
            assert_eq!(a, z, "a section end's claim takes no partition");
        }
    }
}
