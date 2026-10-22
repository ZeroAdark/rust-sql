//! The spool face (lower.md §15.17) on the kernels rig: a producer pipeline spools generated rows into a Materialize
//! breaker's run stream, a consumer reads them back through the forward reader — by-value and by-reference columns,
//! one run and several (the put at a full run), the sink's stop at every block, mark / restore inside the buffer and
//! across runs, both backends; the runs byte-equal to `encode_row`'s and the IR text stable across builds.

mod common;

use common::krig;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_abi::ctx::{hdr, resume};
use gravity_abi::status::DONE;
use gravity_expr::byref;
use gravity_ir::builder::ProgramBuilder;
use gravity_ir::{ConvOp, Func, IntBinOp, IntCond, Type};
use gravity_lower::rerun::spool;
use gravity_lower::rerun::spool::Col;
use gravity_lower::{assemble, compile, Backend, Descs, Lowered};
use gravity_program::{Arena, ArenaVec, End, Layout, OutCol, Pipeline, Program, Reason, WritePlan};

const SPOOL: Breaker = Breaker { kind: BreakerKind::Materialize, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Runs };
const P_N: i32 = 0;
const P_MARK: i32 = 8;
const P_RESTORE: i32 = 16;
const P_IMG: i32 = 24;

fn producer_ir(pb: &mut ProgramBuilder, by_ref: bool) -> Func {
    let p = spool::declare_producer(pb);
    let f = pb.declare_func("spool_fill_rows", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let ctx = b.params()[0];
    let l_start = b.create_block(&[]);
    let l_loop = b.create_block(&[Type::I64]);
    let l_body = b.create_block(&[]);
    let l_end = b.create_block(&[]);
    let l_exit = b.create_block(&[Type::I64]);
    b.mark_loop_header(l_loop);
    let zero = b.iconst(Type::I64, 0);
    let one = b.iconst(Type::I64, 1);
    let done = b.iconst(Type::I64, DONE);
    let r0 = spool::open(&mut b, &p, ctx);
    let none = b.icmp(IntCond::Eq, r0, zero);
    let st = b.select(none, done, r0);
    let le = b.icmp(IntCond::Sle, r0, zero);
    b.cond_br(le, (l_exit, &[st]), (l_start, &[]));
    b.switch_to(l_start);
    let run = b.conv(ConvOp::IntToPtr, Type::Ptr, r0);
    let params = b.load(Type::Ptr, ctx, hdr::PARAMS as i32);
    let n = b.load(Type::I64, params, P_N);
    b.br(l_loop, &[zero]);
    b.switch_to(l_loop);
    let r = b.block_params(l_loop)[0];
    let at_end = b.icmp(IntCond::Uge, r, n);
    b.cond_br(at_end, (l_end, &[]), (l_body, &[]));
    b.switch_to(l_body);
    let three = b.iconst(Type::I64, 3);
    let seven = b.iconst(Type::I64, 7);
    let r3 = b.int_bin(IntBinOp::Mul, r, three);
    let v0 = b.int_bin(IntBinOp::Sub, r3, seven);
    let low = b.int_bin(IntBinOp::And, r, three);
    let valid1 = b.icmp(IntCond::Ne, low, zero);
    let t = b.bconst(true);
    let mut cols = vec![Col { value: v0, valid: t, typlen: None }, Col { value: r, valid: valid1, typlen: None }];
    if by_ref {
        let half = b.int_bin(IntBinOp::Ushr, r, one);
        let idx = b.int_bin(IntBinOp::And, half, three);
        let ip = b.ptr_add(params, idx, 8, P_IMG);
        let img = b.load(Type::I64, ip, 0);
        let low7 = b.int_bin(IntBinOp::And, r, seven);
        let valid2 = b.icmp(IntCond::Ne, low7, zero);
        cols.push(Col { value: img, valid: valid2, typlen: Some(-1) });
    }
    spool::push(&mut b, &p, ctx, run, &cols, l_exit);
    let r1 = b.int_bin(IntBinOp::Add, r, one);
    b.br(l_loop, &[r1]);
    b.switch_to(l_end);
    spool::finish(&mut b, &p, ctx, run, l_exit);
    b.br(l_exit, &[done]);
    b.switch_to(l_exit);
    let s = b.block_params(l_exit)[0];
    b.ret(&[s]);
    drop(b);
    f
}

/// Emits `(c0, c1[, len(c2), last byte of c2])`; marks at row `P_MARK`, restores once before row `P_RESTORE`.
fn consumer_ir(pb: &mut ProgramBuilder, by_ref: bool) -> Func {
    let h = spool::declare_reader(pb);
    let sink = spool::declare_sink(pb);
    let f = pb.declare_func("spool_replay", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let ctx = b.params()[0];
    let natts = if by_ref { 3 } else { 2 };
    let nout = if by_ref { 4 } else { 2 };
    let l_row = b.create_block(&[]);
    let l_mark = b.create_block(&[]);
    let l_marked = b.create_block(&[]);
    let l_restore = b.create_block(&[]);
    let l_read = b.create_block(&[]);
    let l_emit = b.create_block(&[]);
    let l_stop = b.create_block(&[Type::I64]);
    let l_susp = b.create_block(&[]);
    let l_done = b.create_block(&[]);
    let l_exit = b.create_block(&[Type::I64]);
    b.mark_loop_header(l_row);
    let zero = b.iconst(Type::I64, 0);
    let one = b.iconst(Type::I64, 1);
    let vals = b.declare_stack_slot(8 * 4, 8);
    let nulls = b.declare_stack_slot(8, 8);
    let state = b.declare_stack_slot(8 * 4, 8);
    let (ov, on, stp) = (b.stack_addr(vals), b.stack_addr(nulls), b.stack_addr(state));
    // state: [count, restored, mark run, mark off]
    for k in 0..4 {
        b.store(stp, k * 8, zero);
    }
    let rd = spool::reader_open(&mut b, &h, ctx, 0, 1, l_exit);
    let params = b.load(Type::Ptr, ctx, hdr::PARAMS as i32);
    let mark_at = b.load(Type::I64, params, P_MARK);
    let restore_at = b.load(Type::I64, params, P_RESTORE);
    b.br(l_row, &[]);

    b.switch_to(l_row);
    let count = b.load(Type::I64, stp, 0);
    let is_mark = b.icmp(IntCond::Eq, count, mark_at);
    b.cond_br(is_mark, (l_mark, &[]), (l_marked, &[]));
    b.switch_to(l_mark);
    let (mr, mo) = spool::mark(&mut b, rd);
    b.store(stp, 16, mr);
    b.store(stp, 24, mo);
    b.br(l_marked, &[]);
    b.switch_to(l_marked);
    let restored = b.load(Type::I64, stp, 8);
    let fresh = b.icmp(IntCond::Eq, restored, zero);
    let due = b.icmp(IntCond::Eq, count, restore_at);
    let go = b.select(fresh, due, fresh);
    b.cond_br(go, (l_restore, &[]), (l_read, &[]));
    b.switch_to(l_restore);
    b.store(stp, 8, one);
    let (mr, mo) = (b.load(Type::I64, stp, 16), b.load(Type::I64, stp, 24));
    spool::restore(&mut b, &h, ctx, rd, mr, mo, l_exit);
    b.br(l_read, &[]);

    b.switch_to(l_read);
    let (has, row) = spool::next(&mut b, &h, ctx, rd, l_exit);
    b.cond_br(has, (l_emit, &[]), (l_done, &[]));
    b.switch_to(l_emit);
    let count = b.load(Type::I64, stp, 0);
    let c1 = b.int_bin(IntBinOp::Add, count, one);
    b.store(stp, 0, c1);
    b.store(on, 0, zero);
    for k in 0..2 {
        let (v, valid) = spool::col(&mut b, row, natts, k, false);
        let nb = b.select(valid, zero, one);
        let nb8 = b.conv(ConvOp::Trunc, Type::I8, nb);
        b.store(ov, (k * 8) as i32, v);
        b.store(on, k as i32, nb8);
    }
    if by_ref {
        let (addr, valid) = spool::col(&mut b, row, natts, 2, true);
        let rowi = b.conv(ConvOp::PtrToInt, Type::I64, row);
        let safe = b.select(valid, addr, rowi);
        let p = b.conv(ConvOp::IntToPtr, Type::Ptr, safe);
        let len = byref::image_len(&mut b, p, -1);
        let lastp = b.ptr_add(p, len, 1, -1);
        let last = b.load_zext_unaligned(Type::I8, lastp, 0);
        let nb = b.select(valid, zero, one);
        let nb8 = b.conv(ConvOp::Trunc, Type::I8, nb);
        let lv = b.select(valid, len, zero);
        let bv = b.select(valid, last, zero);
        b.store(ov, 16, lv);
        b.store(ov, 24, bv);
        b.store(on, 2, nb8);
        b.store(on, 3, nb8);
    }
    let nc = b.iconst(Type::I64, nout as i64);
    let fl = b.call_host(sink.emit, &[ctx, ov, on, nc], Some(Type::I64)).unwrap();
    let cont = b.icmp(IntCond::Eq, fl, zero);
    b.cond_br(cont, (l_row, &[]), (l_stop, &[fl]));

    b.switch_to(l_stop);
    let fl = b.block_params(l_stop)[0];
    let word = b.iconst(Type::I64, resume::word(resume::EMIT, 1));
    b.store(ctx, hdr::RESUME as i32, word);
    let bad = b.icmp(IntCond::Slt, fl, zero);
    b.cond_br(bad, (l_exit, &[fl]), (l_susp, &[]));
    b.switch_to(l_susp);
    let s = b.call_host(sink.stopped, &[ctx], Some(Type::I64)).unwrap();
    b.br(l_exit, &[s]);
    b.switch_to(l_done);
    let s = b.call_host(sink.done, &[ctx], Some(Type::I64)).unwrap();
    b.br(l_exit, &[s]);
    b.switch_to(l_exit);
    let s = b.block_params(l_exit)[0];
    b.ret(&[s]);
    drop(b);
    f
}

fn lowered(arena: &Arena, by_ref: bool) -> Lowered {
    let a = arena.handle();
    let mut pb = ProgramBuilder::new();
    let (fp, fc) = (producer_ir(&mut pb, by_ref), consumer_ir(&mut pb, by_ref));
    let mut funcs = ArenaVec::with_capacity_in(2, a);
    funcs.push(fp);
    funcs.push(fc);
    let mut out = ArenaVec::with_capacity_in(4, a);
    for name in ["a", "b", "len", "last"].iter().take(if by_ref { 4 } else { 2 }) {
        out.push(OutCol::new(name, 20, -1, 8, true));
    }
    let mut pipelines = ArenaVec::with_capacity_in(2, a);
    pipelines.push(Pipeline::serial(-1, End::Breaker(SPOOL), Reason::NoDrivingSite));
    pipelines.push(Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(0));
    let layout = Layout { nsites: 0, out_natts: out.len() as u16, out_rows: 16, scratch_bytes: 0, exec_bytes: 0 };
    Lowered { ir: pb.finish(), funcs, tagged: Vec::new(), staged: gravity_fn::arena::ChargedVec::none(), descs: Descs { layout, sites: ArenaVec::with_capacity_in(0, a), out, write: WritePlan::SELECT, pipelines, out_region: 0, owner: gravity_lower::no_owner(), no_cache: false, no_cache_why: "", instr: Vec::new(), v4: Default::default(), used: 0, locks: Vec::new(), writes: Vec::new(), write_end: gravity_program::program::END_STATEMENT }, key: None }
}

fn program(arena: &Arena, by_ref: bool, backend: Backend) -> Program {
    let l = lowered(arena, by_ref);
    gravity_ir::verify::verify(&l.ir).unwrap_or_else(|e| panic!("{e:?}\n{}", gravity_ir::text::print_program(&l.ir)));
    gravity_ir::verify::verify_dominance(&l.ir).unwrap_or_else(|e| panic!("{e:?}\n{}", gravity_ir::text::print_program(&l.ir)));
    let codes = compile(&l, backend).unwrap_or_else(|e| panic!("backend: {}", e.message()));
    assemble(&l, codes)
}

/// Four varlena images in 8-aligned storage: 1-byte headers of 4, 9 and 21 bytes, a 4-byte header of 40.
fn images() -> Vec<Vec<u64>> {
    [(4usize, true), (9, true), (40, false), (21, true)].iter().enumerate().map(|(i, &(len, short))| {
        let mut bytes = vec![0u8; len.div_ceil(8) * 8 + 8];
        let hdr = if short { bytes[0] = ((len as u8) << 1) | 1; 1 } else { bytes[..4].copy_from_slice(&((len as u32) << 2).to_le_bytes()); 4 };
        for (j, x) in bytes[hdr..len].iter_mut().enumerate() {
            *x = (i * 50 + j + 1) as u8;
        }
        bytes.chunks(8).map(|c| u64::from_ne_bytes(c.try_into().unwrap())).collect()
    }).collect()
}

const IMG_LEN: [usize; 4] = [4, 9, 40, 21];

fn image_bytes(imgs: &[Vec<u64>], i: usize) -> &[u8] {
    // SAFETY: the image's words hold at least `IMG_LEN[i]` bytes.
    unsafe { core::slice::from_raw_parts(imgs[i].as_ptr() as *const u8, IMG_LEN[i]) }
}

/// Row `r` as the producer computes it: `(values, nulls, image index)`.
fn source_row(r: u64, by_ref: bool) -> (Vec<u64>, Vec<u8>, Option<usize>) {
    let mut v = vec![(r as i64 * 3 - 7) as u64, r];
    let mut n = vec![0u8, (r & 3 == 0) as u8];
    let img = by_ref.then(|| ((r >> 1) & 3) as usize);
    if by_ref {
        v.push(0);
        n.push((r & 7 == 0) as u8);
    }
    (v, n, img)
}

fn want_row(r: u64, by_ref: bool, imgs: &[Vec<u64>]) -> (Vec<u64>, Vec<u8>) {
    let (v, n, img) = source_row(r, by_ref);
    let mut ov = vec![v[0], if n[1] != 0 { 0 } else { v[1] }];
    let mut on = vec![0u8, n[1]];
    if let Some(i) = img {
        let null = n[2] != 0;
        let b = image_bytes(imgs, i);
        ov.extend(if null { [0, 0] } else { [b.len() as u64, *b.last().unwrap() as u64] });
        on.extend([n[2], n[2]]);
    }
    (ov, on)
}

struct Case {
    n: u64,
    by_ref: bool,
    stop: bool,
    mark: u64,
    restore: u64,
    /// The producer reads each image from a copy whose last byte is a mapping's last byte (the next page PROT_NONE).
    guard: bool,
}

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn getpagesize() -> i32;
}

/// Each image's bytes copied to the end of its own two-page mapping's first page, the second PROT_NONE: the copies'
/// addresses and the mappings (`(base, len)`, for `munmap`).
fn guarded(imgs: &[Vec<u64>]) -> (Vec<u64>, Vec<(*mut u8, usize)>) {
    let (mut at, mut maps) = (Vec::new(), Vec::new());
    for i in 0..imgs.len() {
        let b = image_bytes(imgs, i);
        // SAFETY: a private anonymous two-page mapping, the second page made inaccessible; the image's bytes end at the
        // first page's last byte. Unmapped by the caller.
        unsafe {
            let page = getpagesize() as usize;
            let map_anon = if cfg!(target_os = "macos") { 0x1000 } else { 0x20 };
            let base = mmap(core::ptr::null_mut(), 2 * page, 3, 0x02 | map_anon, -1, 0);
            assert!(!base.is_null() && base as isize != -1, "mmap");
            assert_eq!(mprotect(base.add(page), page, 0), 0, "mprotect");
            let p = base.add(page - b.len());
            core::ptr::copy_nonoverlapping(b.as_ptr(), p, b.len());
            at.push(p as u64);
            maps.push((base, 2 * page));
        }
    }
    (at, maps)
}

fn run_case(c: &Case, backend: Backend) -> krig::Rig {
    let arena = Arena::new(1 << 22);
    let imgs = images();
    let mut params = vec![c.n, c.mark, c.restore];
    let (at, maps) = if c.guard { guarded(&imgs) } else { (imgs.iter().map(|i| i.as_ptr() as u64).collect(), Vec::new()) };
    params.extend(at);
    let mut rig = krig::Rig::new(program(&arena, c.by_ref, backend), params);
    rig.host.stop_after_rows = c.stop.then_some(1);
    let (rows, rets) = rig.run_pipelines();
    let tag = format!("n {} by_ref {} stop {} mark {} restore {}", c.n, c.by_ref, c.stop, c.mark, c.restore);
    assert!(rets.iter().all(|r| matches!(r, krig::Ret::Done | krig::Ret::Suspended { .. })), "{tag}: {rets:?}");
    assert_eq!(rets.last(), Some(&krig::Ret::Done), "{tag}");
    if c.stop && c.n > 16 {
        assert!(rets.iter().filter(|r| matches!(r, krig::Ret::Suspended { .. })).count() as u64 >= c.n / 16 - 1, "{tag}: the sink stopped every block");
    }
    // the runs are the rows in `encode_row`'s format, in order, position-independent
    let mut oracle = Vec::new();
    for r in 0..c.n {
        let (v, n, img) = source_row(r, c.by_ref);
        let mut im = vec![None; v.len()];
        if let Some(i) = img {
            im[2] = Some(image_bytes(&imgs, i));
        }
        spool::encode_row(&mut oracle, &v, &n, &im);
    }
    let put: Vec<u8> = rig.host.runs.concat();
    assert!(put == oracle, "{tag}: the runs' bytes ({} in {} runs) are not the encoded rows ({})", put.len(), rig.host.runs.len(), oracle.len());
    let order: Vec<u64> = if c.restore < c.n { (0..c.restore).chain(c.mark..c.n).collect() } else { (0..c.n).collect() };
    assert_eq!(rows.len(), order.len(), "{tag}");
    for (i, &r) in order.iter().enumerate() {
        let (wv, wn) = want_row(r, c.by_ref, &imgs);
        assert_eq!((&rows[i], &rig.host.pushed_nulls[i]), (&wv, &wn), "{tag}: out row {i} (source row {r})");
    }
    for (base, len) in maps {
        // SAFETY: a mapping `guarded` made; the run is done with its images.
        assert_eq!(unsafe { munmap(base, len) }, 0);
    }
    rig
}

const NONE: u64 = u64::MAX;

#[test]
fn a_spool_reads_back_what_was_pushed_at_every_shape() {
    for backend in [Backend::Interp, Backend::Tpde] {
        for by_ref in [false, true] {
            for n in [0u64, 1, 16, 17, 1000] {
                for stop in [false, true] {
                    let rig = run_case(&Case { n, by_ref, stop, mark: NONE, restore: NONE, guard: false }, backend);
                    assert_eq!(rig.host.runs.len(), (n > 0) as usize, "one run without a full buffer");
                }
            }
        }
    }
}

/// D3661: each image's last byte a mapping's last byte (images of 4, 9, 40 and 21 bytes): the spool copies no byte past
/// an image (that read is a SIGSEGV), and the runs still byte-equal `encode_row`'s.
#[test]
fn an_image_ending_its_mapping_is_spooled_without_reading_past_it() {
    for backend in [Backend::Interp, Backend::Tpde] {
        run_case(&Case { n: 64, by_ref: true, stop: false, mark: NONE, restore: NONE, guard: true }, backend);
    }
}

#[test]
fn a_full_run_is_put_and_the_reader_crosses_runs_with_one_host_call_per_buffer() {
    for backend in [Backend::Interp, Backend::Tpde] {
        for (by_ref, stop) in [(false, false), (true, false), (true, true)] {
            let n = 60_000;
            let rig = run_case(&Case { n, by_ref, stop, mark: NONE, restore: NONE, guard: false }, backend);
            let bytes: usize = rig.host.runs.iter().map(Vec::len).sum();
            assert!(rig.host.runs.len() >= 2 && rig.host.runs.len() == bytes.div_ceil(krig::RIG_SHARE as usize - 16), "{} runs for {bytes} bytes: a put per full run", rig.host.runs.len());
            assert_eq!(rig.host.run_ops.0, rig.host.runs.len(), "each run opened once");
            assert!(rig.host.run_ops.1 <= bytes / 0xfff8 + 2 * rig.host.runs.len() + 2, "{} fills for {bytes} bytes: per buffer, never per row", rig.host.run_ops.1);
        }
    }
}

#[test]
fn mark_and_restore_inside_the_buffer_and_across_runs() {
    for backend in [Backend::Interp, Backend::Tpde] {
        for by_ref in [false, true] {
            // an equal-key group read twice inside the buffer: no reopen
            let rig = run_case(&Case { n: 100, by_ref, stop: false, mark: 10, restore: 20, guard: false }, backend);
            assert_eq!(rig.host.run_ops.0, 1, "an in-buffer restore moves the cursor only");
            // back to the first run from the last: the run reopened and read forward
            let rig = run_case(&Case { n: 60_000, by_ref, stop: false, mark: 5, restore: 59_990, guard: false }, backend);
            assert!(rig.host.run_ops.0 > rig.host.runs.len(), "a restore across runs reopens");
        }
    }
}

#[test]
fn the_ir_text_is_stable_across_builds() {
    let (a1, a2) = (Arena::new(1 << 22), Arena::new(1 << 22));
    for by_ref in [false, true] {
        let (x, y) = (lowered(&a1, by_ref), lowered(&a2, by_ref));
        assert_eq!(gravity_ir::text::print_program(&x.ir), gravity_ir::text::print_program(&y.ir));
    }
}
