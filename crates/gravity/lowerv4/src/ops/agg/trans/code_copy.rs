//! Min / max over a Code lane: a record slot's copy of its extreme. Hooks / levers: -. Budget 220 lines
//! (files.tsv, +25 % gate). Owner M1-FOLD.
//!
//! Split from class_minmax_code.rs by responsibility (D267, D277): the record side of `class.minmax_code`. A slot holds
//! one copy of its group's extreme, overwritten in place (else copied at twice the capacity), with the winner's code and
//! a per-part losing threshold in its header, so a row of a known part folds without its image and a group's arena stays
//! bounded by its largest image whatever the rows. The copies pack into the keyed end's image cursor (`step_with`), else
//! a pinned region each. Registers nothing; class_minmax_code.rs emits through it. D1750: a copy merged into a carry
//! (`fold_image`) is read where it lies, never copied.
mod raw;

use gravity_expr::emit::CellValid;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_kernels::dict::strategies::percode::HDR;
use mcx::{CollectIn, PgVec};

use crate::frame::env::Env;
use raw::{fold_copy, fold_row, pick};
/// N13: the copies' allocator, class_num's side copies' too (its raw.rs `num_side_step`).
pub(crate) use raw::alloc as image_alloc;

/// A record slot's copy of its extreme: the varlena the slot points at, behind a header of three words (8-aligned): the
/// varlena's capacity, the winner's `part << 32 | code`, and the losing threshold (`NO_THR`, or `part << 32 | code`: a row
/// of that part no better than it loses to the winner, D277). The copy is this class's own for the record: an
/// improvement overwrites it in place when it fits (else a copy of twice the capacity), so a group holds at most about
/// twice its largest image whatever the rows or improvements, and `Rec::fold` (abi `fold_var`) reads a plain image. The
/// header and the chunks behind a cursor (the first 64 KiB, doubling to 32 MiB) are the per-code front's own (host_abi
/// m1_percode.rs), whose flush leaves its slots under this rule.
const WIN: i32 = 8 - HDR as i32;

/// The row word `word` (its image through site / lane `at` = `site << 8 | lane`) folded into the record slot at
/// `base + off` where `when` holds. A row of the winner's part compares codes here and calls the host only on an
/// improvement (a part's dictionary is byte-rank sorted); any other row asks the host. `images`: the end's image cursor
/// (`cur, end, chunk`), `None` for a pin per copy (the plain fold's one record). The pinned pool's refusal raises.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fold_word(b: &mut FunctionBuilder<'_>, env: &Env, base: Value, off: i32, valid: CellValid, word: Value, at: Value, min: bool, when: Value, images: Option<Value>) {
    let (l_chk, l_cmp, l_call, l_out) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    b.cond_br(when, (l_chk, &[]), (l_out, &[]));
    b.switch_to(l_chk);
    let p = b.load(Type::I64, base, off);
    let some = b.icmp(IntCond::Ne, p, env.zero);
    b.cond_br(some, (l_cmp, &[]), (l_call, &[]));
    b.switch_to(l_cmp);
    let pp = b.conv(ConvOp::IntToPtr, Type::Ptr, p);
    let w = b.load(Type::I64, pp, WIN);
    let s32 = b.iconst(Type::I64, 32);
    let (wp, rp) = (b.int_bin(IntBinOp::Ushr, w, s32), b.int_bin(IntBinOp::Ushr, word, s32));
    let same = b.icmp(IntCond::Eq, wp, rp);
    let m = b.iconst(Type::I64, u32::MAX as i64);
    let (wc, rc) = (b.int_bin(IntBinOp::And, w, m), b.int_bin(IntBinOp::And, word, m));
    let gain = b.icmp(if min { IntCond::Ult } else { IntCond::Ugt }, rc, wc);
    let coded = b.icmp(IntCond::Ne, wc, m);
    let t = b.bconst(true);
    let no_gain = b.int_bin(IntBinOp::Xor, gain, t);
    let skip = b.int_bin(IntBinOp::And, same, no_gain);
    let skip = b.int_bin(IntBinOp::And, skip, coded);
    b.cond_br(skip, (l_out, &[]), (l_call, &[]));
    b.switch_to(l_call);
    let r = call(b, env, ("code_copy::fold_row", gravity_abi::host_words!(fold_row(ctx, slot, images, at, word, min))), (base, off), images, &[at, word], min);
    stored(b, env, base, valid, r, l_out);
}

/// A record slot's copy folded into another's (a record merge: `into ← from`, each `(base, off, valid)`) where `from`
/// holds one.
pub(crate) fn fold_slot(b: &mut FunctionBuilder<'_>, env: &Env, into: (Value, i32, CellValid), from: (Value, i32, CellValid), min: bool, images: Option<Value>) {
    let ((base, off, valid), (fb, fo, fv)) = (into, from);
    let p = b.load(Type::I64, fb, fo);
    let ok = crate::ops::shared::slots::slot_ok(b, env, fb, fv);
    let (l_do, l_out) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(ok, (l_do, &[]), (l_out, &[]));
    b.switch_to(l_do);
    let r = call(b, env, ("code_copy::fold_copy", gravity_abi::host_words!(fold_copy(ctx, slot, images, src, min))), (base, off), images, &[p], min);
    stored(b, env, base, valid, r, l_out);
}

/// A carry's `where` word when it holds a record copy's address, not a code (D1750; a site's `site << 8 | lane` is
/// never negative).
pub(crate) const IMAGE: i64 = -1;

/// The record slot's copy `from` (`(base, off, valid)`) merged into the carry `w` (`[word, where, n]`, D1750:
/// keyed_distinct_plain's walk over its records) where it holds one: the carry keeps the better one's address by bytes
/// (`pick`), `where` = `IMAGE`.
pub(crate) fn fold_image(b: &mut FunctionBuilder<'_>, env: &Env, w: &mut [Value], (fb, fo, fv): (Value, i32, CellValid), min: bool) {
    let p = b.load(Type::I64, fb, fo);
    let (set, some) = (crate::ops::shared::slots::slot_ok(b, env, fb, fv), b.icmp(IntCond::Ne, p, env.zero));
    let ok = b.int_bin(IntBinOp::And, set, some);
    let (l_do, l_ok, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64; 3]));
    b.cond_br(ok, (l_do, &[]), (l_join, &*w));
    b.switch_to(l_do);
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    let h = b.declare_host_once("code_copy::pick", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64], Some(Type::I64), pick as *const () as usize, flags);
    let mv = b.iconst(Type::I64, i64::from(min));
    let r = b.call_host(h, &[env.ctx, w[0], w[1], w[2], p, mv], Some(Type::I64)).expect("returns an address");
    let failed = b.icmp(IntCond::Eq, r, env.zero);
    let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
    b.switch_to(l_ok);
    let (image, n) = (b.iconst(Type::I64, IMAGE), b.int_bin(IntBinOp::Add, w[2], env.one));
    b.br(l_join, &[r, image, n]);
    b.switch_to(l_join);
    w.copy_from_slice(b.block_params(l_join));
}

/// Fold host `host` (`(name, address)`) over `(ctx, slot, cursor, words…, min)` for the slot at `base + off`: its status.
fn call(b: &mut FunctionBuilder<'_>, env: &Env, host: (&'static str, usize), (base, off): (Value, i32), images: Option<Value>, words: &[Value], min: bool) -> Value {
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    let params = [Type::Ptr, Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64];
    let h = b.declare_host_once(host.0, &params[..words.len() + 4], Some(Type::I64), host.1, flags);
    let slot = b.ptr_add(base, env.zero, 1, off);
    let cur = match images {
        Some(v) => v,
        None => b.iconst(Type::Ptr, 0),
    };
    let mv = b.iconst(Type::I64, i64::from(min));
    let args: PgVec<'_, Value> = [env.ctx, slot, cur].into_iter().chain(words.iter().copied()).chain([mv]).collect_in(env.mcx);
    b.call_host(h, &args[..], Some(Type::I64)).expect("returns its status")
}

/// After a fold host's status `r`: 0 raises the stashed error, 1 marks the slot valid (it holds an image), 2 (nothing
/// stored: a code with no image renders NULL) leaves it; then `l_out`.
fn stored(b: &mut FunctionBuilder<'_>, env: &Env, base: Value, valid: CellValid, r: Value, l_out: gravity_ir::Block) {
    let failed = b.icmp(IntCond::Eq, r, env.zero);
    let l_ok = b.create_block(&[]);
    let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
    b.switch_to(l_ok);
    let one = b.icmp(IntCond::Eq, r, env.one);
    crate::ops::shared::slots::mark(b, env, base, valid, one);
    b.br(l_out, &[]);
    b.switch_to(l_out);
}

/// Whether code `x` is better than `y` (the same part: byte-rank order).
fn gains(min: bool, x: u32, y: u32) -> bool {
    if min { x < y } else { x > y }
}

#[cfg(test)]
#[allow(unsafe_code)] // the tests drive the raw half's copies directly
mod tests {
    use super::*;
    use super::raw::{loses, put, write_image};
    use gravity_abi::breaker::T1_IMAGE_CHUNK;
    use gravity_abi::Ctx;

    thread_local! {
        static PINS: core::cell::RefCell<Vec<Vec<u64>>> = const { core::cell::RefCell::new(Vec::new()) };
    }

    /// The test's `Program.pin`: a zeroed region of its own, kept to the thread's end.
    unsafe extern "C" fn test_pin(_ctx: *mut Ctx, bytes: u64) -> *mut u8 {
        PINS.with(|p| {
            let mut v = vec![0u64; (bytes as usize).div_ceil(8)];
            let d = v.as_mut_ptr() as *mut u8;
            p.borrow_mut().push(v);
            d
        })
    }

    /// `(pins, pinned bytes)` so far on this thread.
    fn pinned() -> (usize, usize) {
        PINS.with(|p| (p.borrow().len(), p.borrow().iter().map(|v| v.len() * 8).sum()))
    }

    /// A bound ctx whose program pins through `test_pin` (the ctx and its program kept alive by the caller).
    fn test_ctx() -> (Vec<u64>, Box<gravity_abi::ProgramBlock>) {
        let mut program = Box::new(gravity_abi::ProgramBlock::NONE);
        program.pin = Some(test_pin);
        let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(1) / 8];
        // SAFETY: the words are a ctx image; `program` outlives every use.
        unsafe { (*(ctx.as_mut_ptr() as *mut Ctx)).program = &mut *program };
        (ctx, program)
    }

    /// `fold_row` without its render: a row of `part` whose code is `code` and whose bytes are `bytes`.
    unsafe fn row(ctx: *mut Ctx, slot: &mut u64, images: *mut u64, part: u32, code: u32, bytes: &[u8], min: bool) {
        let word = (part as u64) << 32 | code as u64;
        if *slot == 0 || !loses(*slot, word, min) {
            assert_eq!(put(ctx, slot, images, bytes, word, min), 1);
        }
    }

    fn image(slot: u64) -> Vec<u8> {
        unsafe { gravity_kernels::dict::reps::varlena_bytes(slot as usize as *const u8) }.to_vec()
    }

    /// Strings whose byte order is `0..n`'s, of lengths 1 to 96 (a part's dictionary: code `i` is the `i`-th).
    fn dict(n: usize) -> Vec<Vec<u8>> {
        let mut v: Vec<Vec<u8>> = (0..n).map(|i| format!("{:05}{}", i, "u".repeat(i % 91)).into_bytes()).collect();
        v.sort();
        v
    }

    /// D1750: records' copies merged into a carry: the first taken as it lies, then the better by bytes (the empty string
    /// the least); a tie keeps the carry's copy.
    #[test]
    fn d1750_copies_merged_into_a_carry_keep_the_extreme_by_bytes() {
        let strs: [&[u8]; 6] = [b"v2", b"v10", b"w", b"", b"\xff", b"v2"];
        let imgs: Vec<Vec<u64>> = strs.iter().map(|s| {
            let mut v = vec![0u64; 2 + s.len() / 8];
            unsafe { write_image(v.as_mut_ptr() as *mut u8, s) };
            v
        }).collect();
        let ptr = |i: usize| imgs[i].as_ptr() as u64;
        for min in [true, false] {
            let (mut cur, mut at) = (if min { u32::MAX as u64 } else { 0 }, 0u64);
            for i in 0..5 {
                cur = unsafe { pick(0, cur, at, i as u64, ptr(i), min as u64) } as u64;
                at = IMAGE as u64;
            }
            assert_eq!(image(cur), if min { b"".to_vec() } else { b"\xff".to_vec() }, "min {min}");
            let tie = unsafe { pick(0, ptr(0), IMAGE as u64, 1, ptr(5), min as u64) } as u64;
            assert_eq!(tie, ptr(0), "a tie keeps the carry's copy, min {min}");
        }
    }

    /// D277: every row improving (descending under min, ascending under max), in one part and across parts, folds into
    /// one copy per group overwritten in place, at most doubled: the arena is flat in the rows.
    #[test]
    fn a_groups_copies_stay_flat_as_rows_improve() {
        let (mut ctx, _prog) = test_ctx();
        let cp = ctx.as_mut_ptr() as *mut Ctx;
        let d = dict(20_000);
        for min in [true, false] {
            for parts in [1u32, 3] {
                let (mut slot, mut cur) = (0u64, [0u64; 3]);
                let before = pinned();
                let order: Vec<usize> = if min { (0..d.len()).rev().collect() } else { (0..d.len()).collect() };
                for &i in &order {
                    unsafe { row(cp, &mut slot, cur.as_mut_ptr(), (i as u32) % parts, i as u32, &d[i], min) };
                }
                let want = if min { d.iter().min() } else { d.iter().max() }.unwrap();
                assert_eq!(&image(slot), want, "min {min} parts {parts}");
                let (n, bytes) = (pinned().0 - before.0, pinned().1 - before.1);
                assert_eq!(n, 1, "one 64 KiB chunk for the group, min {min} parts {parts}");
                let used = cur[0] - (cur[1] - cur[2]) - gravity_abi::breaker::img_data_off(cur[2] as usize) as u64; // D3312: past the chunk's header and start map
                assert!(used <= 3 * (HDR + 4 * 100) as u64, "the group's copies took {used} B of its chunk, min {min} parts {parts}");
                assert!(bytes <= T1_IMAGE_CHUNK, "{bytes}");
            }
        }
    }

    /// Without a cursor (the plain fold's one record) a copy is a pin of its own, and the doubling keeps them few.
    #[test]
    fn without_a_cursor_the_copies_are_few_pins() {
        let (mut ctx, _prog) = test_ctx();
        let cp = ctx.as_mut_ptr() as *mut Ctx;
        let d = dict(5_000);
        let (mut slot, before) = (0u64, pinned().0);
        for i in (0..d.len()).rev() {
            unsafe { row(cp, &mut slot, core::ptr::null_mut(), 1, i as u32, &d[i], true) };
        }
        assert_eq!(image(slot), d[0]);
        assert!(pinned().0 - before <= 8, "{} pins", pinned().0 - before);
    }

    /// Rows in any order across parts (each part its own byte-sorted dictionary of a subset): the copy is the extreme by
    /// bytes, the threshold's skipped rows never the answer.
    #[test]
    fn cross_part_rows_fold_by_bytes() {
        let (mut ctx, _prog) = test_ctx();
        let cp = ctx.as_mut_ptr() as *mut Ctx;
        let all = dict(3_000);
        // part k holds every string i with i % 4 != k, coded by its rank inside the part
        let parts: Vec<Vec<usize>> = (0..4).map(|k| (0..all.len()).filter(|i| i % 4 != k).collect()).collect();
        let mut x = 88_172_645_463_325_252u64; // xorshift64 seed (not a hash constant: m0_keyhash_only)
        let mut rows = Vec::new();
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let k = (x % 4) as usize;
            let code = ((x >> 8) % parts[k].len() as u64) as usize;
            rows.push((k as u32, code as u32, parts[k][code]));
        }
        for min in [true, false] {
            let (mut slot, mut cur) = (0u64, [0u64; 3]);
            for &(k, code, i) in &rows {
                unsafe { row(cp, &mut slot, cur.as_mut_ptr(), k, code, &all[i], min) };
            }
            let want = rows.iter().map(|r| &all[r.2]);
            let want = if min { want.min() } else { want.max() }.unwrap();
            assert_eq!(&image(slot), want, "min {min}");
        }
    }
}
