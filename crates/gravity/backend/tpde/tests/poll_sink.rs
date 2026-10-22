//! D3651: a cancel check's poll detour (lowerv4 `frame::cancel::check`, D3406: the cold block that calls the
//! `gravity_ir::KERNEL_POLL` host and rejoins the check's continuation) is laid out at the end of its loop, not where the
//! RPO puts it (before the continuation, in the loop's straight line: a grouped count's row loop moved 12 bytes, +5.5 %
//! cycles).
//!
//! Sinking moves the block past the blocks that read the values live through it, so the analyzer extends those values'
//! live ranges to the sunk block. The `kernel_poll` host here clobbers every caller-saved register it may touch: a value
//! live across a poll whose range was not extended sits in a register the call sequence does not save, and the loop's
//! answer comes back wrong.

mod common;
use gravity_ir::text::parse_program;
use gravity_ir::*;

/// The poll's ctx: the check reads `byte` (null: no byte bound) and the poll counts down `left`, answering stop once it
/// runs out; `me` points back at the ctx (the loop reloads it each row: a value of the loop that the poll reads).
#[repr(C)]
struct PollCtx {
    byte: *const u8,
    left: i64,
    polls: i64,
    me: *mut PollCtx,
}

/// Clobbers the caller-saved registers on AArch64 (argument registers x1-x7, the indirect result register x8,
/// temporaries x9-x17, the vector argument and temporary registers).
#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn clobber() {
    unsafe {
        core::arch::asm!(
            "mov x1, #-1", "mov x2, #-1", "mov x3, #-1", "mov x4, #-1", "mov x5, #-1", "mov x6, #-1", "mov x7, #-1", "mov x8, #-1",
            "mov x9, #-1", "mov x10, #-1", "mov x11, #-1", "mov x12, #-1", "mov x13, #-1", "mov x14, #-1", "mov x15, #-1",
            "mov x16, #-1", "mov x17, #-1",
            "movi v0.2d, #0xffffffffffffffff", "movi v1.2d, #0xffffffffffffffff", "movi v2.2d, #0xffffffffffffffff", "movi v3.2d, #0xffffffffffffffff",
            "movi v4.2d, #0xffffffffffffffff", "movi v5.2d, #0xffffffffffffffff", "movi v6.2d, #0xffffffffffffffff", "movi v7.2d, #0xffffffffffffffff",
            "movi v16.2d, #0xffffffffffffffff", "movi v17.2d, #0xffffffffffffffff", "movi v18.2d, #0xffffffffffffffff", "movi v19.2d, #0xffffffffffffffff",
            out("x1") _, out("x2") _, out("x3") _, out("x4") _, out("x5") _, out("x6") _, out("x7") _, out("x8") _,
            out("x9") _, out("x10") _, out("x11") _, out("x12") _, out("x13") _, out("x14") _, out("x15") _,
            out("x16") _, out("x17") _,
            out("v0") _, out("v1") _, out("v2") _, out("v3") _, out("v4") _, out("v5") _, out("v6") _, out("v7") _,
            out("v16") _, out("v17") _, out("v18") _, out("v19") _,
            options(nostack),
        );
    }
}
#[cfg(not(target_arch = "aarch64"))]
#[inline(always)]
fn clobber() {}

/// One poll: 1 = stop, after [`clobber`].
extern "C" fn host_poll(ctx: *mut PollCtx) -> i64 {
    clobber();
    unsafe {
        (*ctx).polls += 1;
        (*ctx).left -= 1;
        ((*ctx).left < 0) as i64
    }
}

/// A row loop shaped as lowerv4 lowers it: a checked header (`header`'s check: the loaded byte pointer, its null test,
/// the byte, the poll detour), and a second check mid-body with `t` live across it. `i`, `acc` (the header's
/// parameters) live through both polls; `t` through the second; `cx` (the ctx reloaded in the row) is the second poll's
/// argument and is read again after it; `a`, `b`, `n`, `ctx` are invariants. A stop answer leaves through `cancel` (-2).
fn src() -> String {
    format!(
        r#"
(program
  (host {poll} (ptr) i64 touches_memory)
  (fn f ((ctx ptr) (n i64) (a i64) (b i64)) (i64)
    (hints (loop head) (cold poll poll2 cancel) (likely head else) (likely read else) (likely body else) (likely read2 else))
    (block entry ((ctx ptr) (n i64) (a i64) (b i64))
      (z (const i64 0))
      (br head z z))
    (block head ((i i64) (acc i64))
      (byte (load ptr ctx 0))
      (null (const ptr 0))
      (unbound (icmp eq byte null))
      (condbr unbound (on) (read)))
    (block read ()
      (v (loadz i8 byte 0))
      (zero (const i64 0))
      (set (icmp ne v zero))
      (condbr set (poll) (on)))
    (block on ()
      (done (icmp sge i n))
      (condbr done (exit acc) (body)))
    (block poll ()
      (st (call_host {poll} ctx))
      (z0 (const i64 0))
      (go (icmp eq st z0))
      (condbr go (on) (cancel)))
    (block body ()
      (t (imul i a))
      (cx (load ptr ctx 24))
      (byte2 (load ptr ctx 0))
      (null2 (const ptr 0))
      (unbound2 (icmp eq byte2 null2))
      (condbr unbound2 (on2) (read2)))
    (block read2 ()
      (v2 (loadz i8 byte2 0))
      (zero2 (const i64 0))
      (set2 (icmp ne v2 zero2))
      (condbr set2 (poll2) (on2)))
    (block on2 ()
      (u (iadd t b))
      (pc (load i64 cx 16))
      (u2 (iadd u pc))
      (acc2 (iadd acc u2))
      (one (const i64 1))
      (i2 (iadd i one))
      (br head i2 acc2))
    (block poll2 ()
      (st2 (call_host {poll} cx))
      (z2 (const i64 0))
      (go2 (icmp eq st2 z2))
      (condbr go2 (on2) (cancel)))
    (block cancel ()
      (m (const i64 -2))
      (return m))
    (block exit ((r i64))
      (return r))))
"#,
        poll = KERNEL_POLL
    )
}

fn load() -> Program {
    parse_program(&src(), &|name| (name == KERNEL_POLL).then_some(host_poll as extern "C" fn(*mut PollCtx) -> i64 as usize)).unwrap_or_else(|e| panic!("{e}"))
}

/// The loop's answer and poll count by hand: two polls per row with a set byte (the header's and the mid-body one) and
/// the header's on the last test; stop once `left` runs out. Each row adds `i * a + b` and the polls so far.
fn expect(n: i64, a: i64, b: i64, set: bool, left: i64) -> (i64, i64) {
    let (mut acc, mut left, mut polls) = (0i64, left, 0i64);
    let mut poll = |polls: &mut i64| {
        *polls += 1;
        left -= 1;
        left < 0
    };
    let mut i = 0i64;
    loop {
        if set && poll(&mut polls) {
            return (-2, polls);
        }
        if i >= n {
            return (acc, polls);
        }
        let t = i.wrapping_mul(a);
        if set && poll(&mut polls) {
            return (-2, polls);
        }
        acc = acc.wrapping_add(t.wrapping_add(b)).wrapping_add(polls);
        i += 1;
    }
}

#[test]
fn values_live_across_a_sunk_poll_keep_their_homes() {
    let p = load();
    let f = Func(0);
    for (n, a, b) in [(0i64, 3i64, 5i64), (1, 3, 5), (7, -11, 13), (100, 7, -2), (1000, 12345, 678)] {
        for (byte, left) in [(None, 0i64), (Some(0u8), 0), (Some(1), i64::MAX / 2), (Some(1), 0), (Some(1), 1), (Some(1), 2 * n), (Some(1), n + 3)] {
            let cell = byte.unwrap_or(0);
            let mut ctx = PollCtx { byte: if byte.is_some() { &cell } else { core::ptr::null() }, left, polls: 0, me: core::ptr::null_mut() };
            ctx.me = core::ptr::addr_of_mut!(ctx);
            let got = common::native(&p, f, &[&mut ctx as *mut PollCtx as u64, n as u64, a as u64, b as u64]);
            let (want, polls) = expect(n, a, b, byte == Some(1), left);
            assert_eq!(got, vec![want as u64], "n={n} a={a} b={b} byte={byte:?} left={left}");
            assert_eq!(ctx.polls, polls, "polls: n={n} byte={byte:?} left={left}");
        }
    }
}

/// The layout: the loop's back edge (`b head`, the first backward unconditional branch) comes before the first call,
/// so neither poll block sits in the loop's straight line. Laid out where the RPO puts them, the header's poll would
/// be the first code after its byte test and its `bl` would come first.
#[cfg(target_arch = "aarch64")]
#[test]
fn poll_blocks_sit_after_the_back_edge() {
    let p = load();
    let ctx = mcx::MemoryContext::new("poll-sink");
    let c = gravity_tpde::compile(ctx.mcx(), &p).unwrap();
    let range = c.mapper().get_mapped_range().unwrap();
    let bytes = unsafe { range.bytes() };
    let off = c.entry(Func(0)) - range.address();
    let (mut back, mut call) = (None, None);
    let mut i = off;
    while i + 4 <= bytes.len() && (back.is_none() || call.is_none()) {
        let w = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        let backward_b = w >> 26 == 0x05 && w & (1 << 25) != 0;
        let is_call = w >> 26 == 0x25 || w & 0xffff_fc1f == 0xd63f_0000;
        if backward_b && back.is_none() {
            back = Some(i - off);
        }
        if is_call && call.is_none() {
            call = Some(i - off);
        }
        i += 4;
    }
    let (back, call) = (back.expect("a back edge"), call.expect("a poll call"));
    assert!(back < call, "the first poll call (+{call:#x}) precedes the loop's back edge (+{back:#x}): a poll block sits in the loop");
}

/// D3896: the second program's ctx: the byte, `w` (the value the walk loop carries), `x` (the fill loop's second
/// round reads it) and the poll's countdown.
#[repr(C)]
struct HandCtx {
    byte: *const u8,
    w: i64,
    x: i64,
    left: i64,
    polls: i64,
}

extern "C" fn host_poll_hand(ctx: *mut HandCtx) -> i64 {
    clobber();
    unsafe {
        (*ctx).polls += 1;
        (*ctx).left -= 1;
        ((*ctx).left < 0) as i64
    }
}

/// D3896, TPC-H Q20's p3 (a hash-chain walk's region pointer handed to the region-write loop): the walk loop's
/// parameter `w` lives through the row's check into `on`, whose edge hands it to the fill loop's `x` as its last
/// reference. The check's poll detour sinks past the fill loop, so `w`'s range runs to it. The anchor edge `on -> fill`
/// may not give `w`'s register to `x`: the detour, compiled after that edge, would not save it around the poll, and
/// its jump back to `on` would hand the poll's leftovers to `x`. Each row adds `w + x`; a stop answer returns -2.
fn src_hand() -> String {
    format!(
        r#"
(program
  (host {poll} (ptr) i64 touches_memory)
  (fn f ((ctx ptr) (n i64)) (i64)
    (hints (loop head walk fill) (cold poll cancel) (likely check else) (likely read else))
    (block entry ((ctx ptr) (n i64))
      (z (const i64 0))
      (br head z z))
    (block head ((i i64) (acc i64))
      (w0 (load i64 ctx 8))
      (k0 (const i64 0))
      (br walk k0 w0))
    (block walk ((k i64) (w i64))
      (one (const i64 1))
      (k1 (iadd k one))
      (two (const i64 2))
      (more (icmp slt k1 two))
      (condbr more (walk k1 w) (check)))
    (block check ()
      (byte (load ptr ctx 0))
      (null (const ptr 0))
      (unbound (icmp eq byte null))
      (condbr unbound (on) (read)))
    (block read ()
      (v (loadz i8 byte 0))
      (zero (const i64 0))
      (set (icmp ne v zero))
      (condbr set (poll) (on)))
    (block poll ()
      (st (call_host {poll} ctx))
      (z0 (const i64 0))
      (go (icmp eq st z0))
      (condbr go (on) (cancel)))
    (block on ()
      (j0 (const i64 0))
      (br fill j0 w acc))
    (block fill ((j i64) (x i64) (a i64))
      (a2 (iadd a x))
      (one2 (const i64 1))
      (j1 (iadd j one2))
      (x1 (load i64 ctx 16))
      (two2 (const i64 2))
      (more2 (icmp slt j1 two2))
      (condbr more2 (fill j1 x1 a2) (tail a2)))
    (block tail ((r i64))
      (one3 (const i64 1))
      (i1 (iadd i one3))
      (again (icmp slt i1 n))
      (condbr again (head i1 r) (exit r)))
    (block cancel ()
      (m (const i64 -2))
      (return m))
    (block exit ((res i64))
      (return res))))
"#,
        poll = KERNEL_POLL
    )
}

fn expect_hand(n: i64, w: i64, x: i64, set: bool, left: i64) -> (i64, i64) {
    let (mut acc, mut left, mut polls, mut i) = (0i64, left, 0i64, 0i64);
    loop {
        if set {
            polls += 1;
            left -= 1;
            if left < 0 {
                return (-2, polls);
            }
        }
        acc = acc.wrapping_add(w).wrapping_add(x);
        i += 1;
        if i >= n {
            return (acc, polls);
        }
    }
}

#[test]
fn a_value_handed_to_a_loop_past_a_sunk_poll_keeps_its_register() {
    let p = parse_program(&src_hand(), &|name| (name == KERNEL_POLL).then_some(host_poll_hand as extern "C" fn(*mut HandCtx) -> i64 as usize)).unwrap_or_else(|e| panic!("{e}"));
    for n in [1i64, 2, 5] {
        for (byte, left) in [(None, 0i64), (Some(0u8), 0), (Some(1), i64::MAX / 2), (Some(1), 0), (Some(1), 2)] {
            let cell = byte.unwrap_or(0);
            let mut ctx = HandCtx { byte: if byte.is_some() { &cell } else { core::ptr::null() }, w: 1000, x: 7, left, polls: 0 };
            let got = common::native(&p, Func(0), &[&mut ctx as *mut HandCtx as u64, n as u64]);
            let (want, polls) = expect_hand(n, 1000, 7, byte == Some(1), left);
            assert_eq!(got, vec![want as u64], "n={n} byte={byte:?} left={left}");
            assert_eq!(ctx.polls, polls, "polls: n={n} byte={byte:?} left={left}");
        }
    }
}
