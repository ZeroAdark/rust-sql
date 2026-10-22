//! rowfilter.rs's verdict on measured costs, road `join.rowfilter.cost` (D3323). Owner M1-JOIN.
//!
//! A tested row that passes pays `ROWFILTER_TEST_NS` (the block test's image, hash and word read, then the walk's own
//! hash); a row the filter rejects saves `ROWFILTER_HEAD_NS` (the directory head past L2 it no longer reads). Each sample
//! of `ROWFILTER_SAMPLE` rows decides: the filter goes off when its rejected rows save no more than its tests cost
//! (rejected x HEAD <= seen x TEST: a pass share of about 65 % and up), and is on otherwise. No verdict is final: once
//! one is in, each participant samples again on one window in `ROWFILTER_RESAMPLE` of its own (the filtered body runs on
//! that window even while off), and each full sample decides again, so a probe whose first rows match (a probe table
//! clustered on the join's key, D3323) arms its filter once later rows stop matching. A window that samples counts its
//! rows in the probe's frame word, folded into the state line at the next window's start; a window that counted nothing
//! never writes the line (the participants share it).
use super::*;

const _: () = assert!(gravity_program::cost::ROWFILTER_RESAMPLE.is_power_of_two());

/// The state line's off word: bit 0 the filter is off, bit 1 a sample has decided.
const OFF: i64 = 1;
const DECIDED: i64 = 2;

/// At the start of a window over a filter whose directory is past L2 (its state line `state`): the rows counted in frame
/// word `word` folded in, a full sample's verdict taken, and the window's (armed, sampling): it samples until the first
/// verdict, then on one window in `ROWFILTER_RESAMPLE` (the participant's window count in frame word `win`), and it is
/// armed while the filter is on or it samples.
pub(super) fn verdict(b: &mut FunctionBuilder<'_>, env: &mut Env, state: Value, (word, win): (i32, i32)) -> (Value, Value) {
    use gravity_program::cost::{ROWFILTER_HEAD_NS, ROWFILTER_RESAMPLE, ROWFILTER_SAMPLE, ROWFILTER_TEST_NS};
    let c = env.load_word(b, word);
    let n = env.load_word(b, win);
    let n = b.int_bin(IntBinOp::Add, n, env.one);
    env.store_word(b, win, n);
    let st0 = b.load(Type::I64, state, ST_OFF);
    let (l_fold, l_decide, l_turn, l_st) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]));
    let counted = b.icmp(IntCond::Ne, c, env.zero);
    b.cond_br(counted, (l_fold, &[]), (l_st, &[st0]));
    b.switch_to(l_fold);
    env.store_word(b, word, env.zero);
    let (s32, low) = (b.iconst(Type::I64, 32), b.iconst(Type::I64, u32::MAX as i64));
    let passed = b.int_bin(IntBinOp::And, c, low);
    let rejected = b.int_bin(IntBinOp::Ushr, c, s32);
    let tested = b.int_bin(IntBinOp::Add, passed, rejected);
    let seen = add_at(b, state, ST_SEEN, tested);
    let pass = add_at(b, state, ST_PASS, passed);
    let sample = b.iconst(Type::I64, ROWFILTER_SAMPLE as i64);
    let enough = b.icmp(IntCond::Uge, seen, sample);
    b.cond_br(enough, (l_decide, &[]), (l_st, &[st0]));
    // a full sample: the verdict, and the next sample starts empty (a lost add between participants only delays it)
    b.switch_to(l_decide);
    let (head, test) = (b.iconst(Type::I64, ROWFILTER_HEAD_NS as i64), b.iconst(Type::I64, ROWFILTER_TEST_NS as i64));
    let rej = b.int_bin(IntBinOp::Sub, seen, pass);
    let saved = b.int_bin(IntBinOp::Mul, rej, head);
    let paid = b.int_bin(IntBinOp::Mul, seen, test);
    let off = b.icmp(IntCond::Ule, saved, paid);
    let off = b.conv(ConvOp::Zext, Type::I64, off);
    let decided = b.iconst(Type::I64, DECIDED);
    let st1 = b.int_bin(IntBinOp::Or, off, decided);
    b.store(state, ST_OFF, st1);
    b.store(state, ST_SEEN, env.zero);
    b.store(state, ST_PASS, env.zero);
    let flip = b.int_bin(IntBinOp::Xor, st0, st1);
    let flip = b.int_bin(IntBinOp::And, flip, env.one);
    let flip = b.icmp(IntCond::Ne, flip, env.zero);
    b.cond_br(flip, (l_turn, &[]), (l_st, &[st1]));
    b.switch_to(l_turn);
    let flags = gravity_ir::HostFlags { touches_memory: true, worker: true, state: gravity_ir::StateMask::NONE, ..gravity_ir::HostFlags::default() };
    let h = b.declare_host_once("rowfilter_turned", &[Type::I64], Some(Type::I64), raw::rowfilter_turned as *const () as usize, flags);
    b.call_host(h, &[st1], Some(Type::I64));
    b.br(l_st, &[st1]);
    b.switch_to(l_st);
    let st = b.block_params(l_st)[0];
    let (mask, dec) = (b.iconst(Type::I64, ROWFILTER_RESAMPLE as i64 - 1), b.iconst(Type::I64, DECIDED));
    let due = b.int_bin(IntBinOp::And, n, mask);
    let due = b.icmp(IntCond::Eq, due, env.zero);
    let d = b.int_bin(IntBinOp::And, st, dec);
    let first = b.icmp(IntCond::Eq, d, env.zero);
    let sampling = b.int_bin(IntBinOp::Or, first, due);
    let o = b.int_bin(IntBinOp::And, st, env.one);
    let on = b.icmp(IntCond::Eq, o, env.zero);
    (b.int_bin(IntBinOp::Or, on, sampling), sampling)
}

/// What `rowfilter_turned` fires for state `st` (raw.rs).
pub(super) fn turned(st: u64) -> &'static str {
    if st & OFF as u64 != 0 {
        "rowfilter.off"
    } else {
        "rowfilter.rearm"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_program::cost::{ROWFILTER_HEAD_NS, ROWFILTER_RESAMPLE, ROWFILTER_SAMPLE, ROWFILTER_TEST_NS};

    /// The rule `verdict` lowers: on while the rejected rows save more than the tests cost.
    fn pays(seen: u64, pass: u64) -> bool {
        (seen - pass) * ROWFILTER_HEAD_NS > seen * ROWFILTER_TEST_NS
    }

    /// One window's start: the state line `st` (seen, pass, off), the frame's count word `c` and window count `n` before
    /// it; answers (armed, sampling), the state line and the frame's two words after it.
    fn run(st: [u64; 3], c: u64, n: u64) -> ((bool, bool), [u64; 3], [u64; 2]) {
        let mut state = [0u64; 8];
        state[..3].copy_from_slice(&st);
        let mut frame = [c, n, 0, 0];
        let (sp, fp) = (state.as_mut_ptr() as i64, frame.as_mut_ptr() as i64);
        let got = crate::frame::stop::tests::run1(&[], &mut |b, env| {
            let at = b.iconst(Type::I64, fp);
            env.words = Some(b.conv(ConvOp::IntToPtr, Type::Ptr, at));
            let at = b.iconst(Type::I64, sp);
            let state = b.conv(ConvOp::IntToPtr, Type::Ptr, at);
            let (armed, sampling) = verdict(b, env, state, (0, 8));
            let (a, s) = (b.conv(ConvOp::Zext, Type::I64, armed), b.conv(ConvOp::Zext, Type::I64, sampling));
            let two = b.iconst(Type::I64, 2);
            let s = b.int_bin(IntBinOp::Mul, s, two);
            b.int_bin(IntBinOp::Or, a, s)
        });
        ((got & 1 != 0, got & 2 != 0), [state[0], state[1], state[2]], [frame[0], frame[1]])
    }

    fn count(pass: u64, rej: u64) -> u64 {
        pass | rej << 32
    }

    /// Until the first verdict every window samples, armed; a window that counted nothing leaves the state line alone.
    #[test]
    fn every_window_samples_until_the_first_verdict() {
        assert_eq!(run([0, 0, 0], 0, 0), ((true, true), [0, 0, 0], [0, 1]), "nothing counted: no state write");
        assert_eq!(run([1000, 900, 0], count(100, 300), 4), ((true, true), [1400, 1000, 0], [0, 5]), "folded, short of a sample");
    }

    /// A full sample decides by measured cost (a pass share past about 65 % turns it off, 26 % keeps it on where the old
    /// 1/4 rule turned it off), starts the next sample empty, and the window samples again only when its count is due.
    #[test]
    fn a_full_sample_decides_by_cost() {
        let s = ROWFILTER_SAMPLE;
        let keep = s * 26 / 100;
        assert!(pays(s, keep) && !pays(s, s * 70 / 100), "26 % passing pays, 70 % does not");
        assert_eq!(run([s - 100, keep - 10, 0], count(10, 90), 0), ((true, false), [0, 0, DECIDED as u64], [0, 1]), "on");
        let most = s * 70 / 100;
        assert_eq!(run([s - 100, most - 100, 0], count(100, 0), 0), ((false, false), [0, 0, (DECIDED | OFF) as u64], [0, 1]), "off");
        // the boundary: rejected rows that save exactly what the tests cost turn it off
        let seen = ROWFILTER_HEAD_NS * 180;
        let rej = ROWFILTER_TEST_NS * 180;
        assert!(!pays(seen, seen - rej) && pays(seen, seen - rej - 1));
        assert_eq!(run([0, 0, 0], count(seen - rej, rej), 0).1[2], (DECIDED | OFF) as u64, "{seen} seen, {rej} rejected");
    }

    /// After a verdict a participant samples on one window in `ROWFILTER_RESAMPLE` of its own, armed then even while off;
    /// a later sample whose rows stop passing arms the filter again (and one that passes turns it off again).
    #[test]
    fn a_decided_filter_samples_again_and_rearms() {
        let (on, off) = ([0, 0, DECIDED as u64], [0, 0, (DECIDED | OFF) as u64]);
        assert_eq!(run(off, 0, 5).0, (false, false), "off between samples");
        assert_eq!(run(off, 0, ROWFILTER_RESAMPLE - 1).0, (true, true), "off, its due window samples armed");
        assert_eq!(run(on, 0, 5).0, (true, false), "on: armed, no count");
        assert_eq!(run(on, 0, 2 * ROWFILTER_RESAMPLE - 1).0, (true, true));
        let s = ROWFILTER_SAMPLE;
        let st = [s - 64, 0, off[2]];
        assert_eq!(run(st, count(0, 64), 3), ((true, false), [0, 0, DECIDED as u64], [0, 4]), "rows stopped passing: armed again");
        let st = [s - 64, s - 64, on[2]];
        assert_eq!(run(st, count(64, 0), 3), ((false, false), off, [0, 4]), "rows pass again: off again");
        assert_eq!((turned(off[2]), turned(on[2])), ("rowfilter.off", "rowfilter.rearm"));
    }
}
