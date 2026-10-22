//! Road `sort.resident` (cards/M3.md L4, H26b, I31): the merge reads worker runs in place, a BufFile only past the grant.
//! A finishing sort pipeline opens its run with `sort_open_res` (`sort_open_res_var` for a var row) and declares
//! `Proto::new(SortMerge, producer).slot(slot)` (a fixed row adds `.arg(spec).peer(nkey)`): at the section end every worker
//! seals its last run where it lies when no run was put and the merged run fits a region, and exec v4/m3_sort.rs merges
//! the sealed runs in place into the slot's run, which `sort_begin` reads as `SORT_MODE_MEM`; else every worker puts, and
//! the emit merges the stream (kernels host_abi/v4/m3_sort.rs). Forced off, the section end puts as `sort_open` does.
//! The put hosts: `sort_full_res` / `sort_last_res` (`_var`) when road sort.keylane or sort.natural marks the run's
//! `pad[2]`, else `sort_full` / `sort_last`. Budget 160 lines. Owner M3-SORT.
use gravity_ir::builder::FunctionBuilder;
use gravity_abi::breaker::SORTRUN_HDR;
use gravity_ir::{Block, ConvOp, HostFlags, HostFn, IntBinOp, IntCond, Type, Value};
use gravity_kernels::host_abi as h;
use gravity_kernels::ops::sort::entries as r;
use gravity_program::protos::{Proto, ProtoKind};

use crate::face::strategy::Strategy;
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::admit::refusal::Variant;
use crate::ops::Table;

crate::strategies! {
    /// Merge reads worker runs in place, BufFile only on spill.
    SORT_RESIDENT = 70, "sort.resident", Sort;
}

pub(crate) const ROWS: Table = Table { strategies: &[(SORT_RESIDENT, &())], ..crate::ops::NONE };

/// Whether road `r` is on for the statement being made (`Env::applies` at make time).
pub(crate) fn on(px: &Px<'_, '_>, r: Strategy) -> bool {
    px.strategies.has(r) && crate::face::source::on(Variant::Strategy(r))
}

pub(crate) fn host(b: &mut FunctionBuilder<'_>, name: &str, params: &[Type], ret: Type, addr: usize) -> HostFn {
    b.declare_host_once(name, params, Some(ret), addr, HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() })
}

/// D463: `sort_total(ctx, tbl, mode)` (`sort_total_at(.., k, slot)` for a reader at its consumed index): the rows the emit sends.
pub(crate) fn total(b: &mut FunctionBuilder<'_>, args: &[Value]) -> Value {
    let h = match args.len() {
        3 => host(b, "sort_total", &[Type::Ptr, Type::Ptr, Type::I64], Type::I64, gravity_abi::host_words!(h::sort_total(ctx, tbl, mode))),
        _ => host(b, "sort_total_at", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64], Type::I64, gravity_abi::host_words!(h::sort_total_at(ctx, tbl, mode, k, slot))),
    };
    b.call_host(h, args, Some(Type::I64)).expect("sort_total returns the rows")
}

/// The run's open: `(ctx, row_bytes, nkey, spec)` for a fixed row, `(ctx, tbl)` for a var row; the run or, at the section
/// end, null / a status below zero.
pub(crate) fn open(b: &mut FunctionBuilder<'_>, var: bool, resident: bool) -> HostFn {
    match (var, resident) {
        (false, false) => host(b, "sort_open", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Type::Ptr, gravity_abi::host_words!(h::sort_open(ctx, row_bytes, nkey, spec))),
        (false, true) => host(b, "sort_open_res", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Type::Ptr, gravity_abi::host_words!(r::sort_open_res(ctx, row_bytes, nkey, spec))),
        (true, false) => host(b, "sort_open_var", &[Type::Ptr, Type::Ptr], Type::Ptr, gravity_abi::host_words!(h::sort_open_var(ctx, tbl))),
        (true, true) => host(b, "sort_open_res_var", &[Type::Ptr, Type::Ptr], Type::Ptr, gravity_abi::host_words!(r::sort_open_res_var(ctx, tbl))),
    }
}

/// D3027 (b): a top-k producer's resident open (topk_rows.rs), `open`'s with its bound and cut flags: `(ctx, row_bytes,
/// nkey, spec, n, flags)` for a fixed row, `(ctx, tbl, n, flags)` for a var row; the section end cuts the run to `n` rows
/// before it seals or puts (kernels `sort_open_res_top`).
pub(crate) fn open_top(b: &mut FunctionBuilder<'_>, var: bool) -> HostFn {
    if var {
        host(b, "sort_open_res_var_top", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64], Type::Ptr, gravity_abi::host_words!(r::sort_open_res_var_top(ctx, tbl, n, flags)))
    } else {
        host(b, "sort_open_res_top", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64], Type::Ptr, gravity_abi::host_words!(r::sort_open_res_top(ctx, row_bytes, nkey, spec, n, flags)))
    }
}

/// A full run put (`last`: a serial producer's last run, sealed when nothing was put): `(ctx, run, nkey, spec)` for a fixed
/// row, `(ctx, run, tbl)` for a var row; `marked`: the run's `pad[2]` bits order it.
pub(crate) fn put(b: &mut FunctionBuilder<'_>, var: bool, last: bool, marked: bool) -> HostFn {
    let fixed = [Type::Ptr, Type::Ptr, Type::I64, Type::I64];
    let v = [Type::Ptr, Type::Ptr, Type::Ptr];
    let (name, addr): (&str, usize) = match (var, last, marked) {
        (false, false, false) => ("sort_full", gravity_abi::host_words!(h::sort_full(ctx, run, nkey, spec))),
        (false, true, false) => ("sort_last", gravity_abi::host_words!(h::sort_last(ctx, run, nkey, spec))),
        (false, false, true) => ("sort_full_res", gravity_abi::host_words!(r::sort_full_res(ctx, run, nkey, spec))),
        (false, true, true) => ("sort_last_res", gravity_abi::host_words!(r::sort_last_res(ctx, run, nkey, spec))),
        (true, false, false) => ("sort_full_var", gravity_abi::host_words!(h::sort_full_var(ctx, run, tbl))),
        (true, true, false) => ("sort_last_var", gravity_abi::host_words!(h::sort_last_var(ctx, run, tbl))),
        (true, false, true) => ("sort_full_res_var", gravity_abi::host_words!(r::sort_full_res_var(ctx, run, tbl))),
        (true, true, true) => ("sort_last_res_var", gravity_abi::host_words!(r::sort_last_res_var(ctx, run, tbl))),
    };
    host(b, name, if var { &v } else { &fixed }, Type::I64, addr)
}

/// A full run's growth in place (`sort_grow(ctx, run, tbl)`, `tbl` null for a fixed row): 1 grew (the row is written
/// again), 0 not (the run is put or cut as before), below zero a status.
pub(crate) fn grow(b: &mut FunctionBuilder<'_>) -> HostFn {
    host(b, "sort_grow", &[Type::Ptr, Type::Ptr, Type::Ptr], Type::I64, gravity_abi::host_words!(r::sort_grow(ctx, run, tbl)))
}

/// The resident merge's protocol for producer `k` into `slot`: a fixed row names its spec and key words.
pub(crate) fn declare(px: &mut Px<'_, '_>, k: usize, slot: u8, fixed: Option<(u64, u16)>) {
    let p = Proto::new(ProtoKind::SortMerge, k as u16).slot(slot);
    px.declare_proto(match fixed {
        Some((spec, nkey)) => p.arg(spec).peer(nkey),
        None => p,
    });
}

/// D1330: a var put's status above zero is the run the next row goes to (`sort_full_var` / `sort_full_res_var`: a big
/// run past the region for a row whose copies an empty run had no room for, then the own run again), stored to the run's
/// word `w_run`, which the append's `l_try` reloads.
pub(crate) fn next_run(b: &mut FunctionBuilder<'_>, env: &Env, w_run: i32, status: Value) {
    let (next, cur) = (b.icmp(IntCond::Sgt, status, env.zero), env.load_word(b, w_run));
    let w = b.select(next, status, cur);
    env.store_word(b, w_run, w);
}

/// A var row's copies into the run (`sort_image_raw`, verbatim whatever the header; a key's tail column, `keyed`, its
/// payload through `sort_image_key`): the record word armed with the row's bytes, then each non-NULL varlena (`vars`:
/// column, slot offset) copied and its slot written; `l_full` when a copy has no room (1: the run is put and the row
/// written again into the emptied run), `l_exit` on a status below zero.
#[allow(clippy::too_many_arguments)]
pub(crate) fn store_vars(b: &mut FunctionBuilder<'_>, env: &Env, run: Value, at: Value, vals: &[(Value, Value)], vars: &[(usize, u32)], keyed: &[usize], row_bytes: u32, rec_off: u32, l_full: Block) {
    let w = b.iconst(Type::I32, row_bytes as i64);
    b.store(at, rec_off as i32, w);
    let img = host(b, "sort_image_raw", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64], Type::I64, gravity_abi::host_words!(h::sort_image_raw(run, ptr, total, slot_off, rec_off)));
    for &(c, off) in vars {
        let (v, null) = vals[c];
        let (l_go, l_null, l_next) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(null, (l_null, &[]), (l_go, &[]));
        b.switch_to(l_null);
        b.store(at, off as i32, env.zero);
        b.br(l_next, &[]);
        b.switch_to(l_go);
        let (so, ro) = (b.iconst(Type::I64, off as i64), b.iconst(Type::I64, rec_off as i64));
        let r = if keyed.contains(&c) {
            let key = host(b, "sort_image_key", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Type::I64, gravity_abi::host_words!(crate::ops::sort::raw::sort_image_key(run, datum, slot_off, rec_off)));
            b.call_host(key, &[run, v, so, ro], Some(Type::I64))
        } else {
            let p = b.conv(ConvOp::IntToPtr, Type::Ptr, v);
            let total = gravity_expr::byref::image_len(b, p, -1);
            b.call_host(img, &[run, p, total, so, ro], Some(Type::I64))
        };
        let r = r.expect("a var copy's host returns its status");
        let (neg, l_room) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
        b.cond_br(neg, (env.x.l_exit, &[r]), (l_room, &[]));
        b.switch_to(l_room);
        let no_room = b.icmp(IntCond::Ne, r, env.zero);
        b.cond_br(no_room, (l_full, &[]), (l_next, &[]));
        b.switch_to(l_next);
    }
}

/// The fixed-width image at datum `ptr` copied into the row at `off` (`byref::copy_fixed`: no byte past it read, its last
/// word's bytes above it zero, D3661); a NULL zeroes it, so NULL rows' tails tie.
pub(crate) fn store_image(b: &mut FunctionBuilder<'_>, env: &Env, at: Value, off: i32, typlen: i16, ptr: Value, valid: Value) {
    let (l_copy, l_null, l_done) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    b.cond_br(valid, (l_copy, &[]), (l_null, &[]));
    let words = (typlen as u32).next_multiple_of(8) as i32 / 8;
    b.switch_to(l_copy);
    let p = b.conv(ConvOp::IntToPtr, Type::Ptr, ptr);
    gravity_expr::byref::copy_fixed(b, at, off, p, typlen);
    b.br(l_done, &[]);
    b.switch_to(l_null);
    for w in 0..words {
        b.store(at, off + 8 * w, env.zero);
    }
    b.br(l_done, &[]);
    b.switch_to(l_done);
}


/// A var run's room for row `len`: full (`full`: `len >= cap`) or when the row would reach the copies above `var_top`
/// (they grow down toward the rows) -> `l_full`, else `l_store`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn var_room(b: &mut FunctionBuilder<'_>, run: Value, len: Value, cap: Value, rb: Value, full: Value, l_full: Block, l_store: Block) {
    let l_room = b.create_block(&[]);
    b.cond_br(full, (l_full, &[]), (l_room, &[]));
    b.switch_to(l_room);
    let end = b.int_bin(IntBinOp::Mul, len, rb);
    let end = b.int_bin(IntBinOp::Add, end, rb);
    let hdr = b.iconst(Type::I64, SORTRUN_HDR as i64);
    let end = b.int_bin(IntBinOp::Add, end, hdr);
    let rows = b.int_bin(IntBinOp::Mul, cap, rb);
    let top_p = b.ptr_add(run, rows, 1, SORTRUN_HDR as i32);
    let top = b.load_zext(Type::I32, top_p, 0);
    let over = b.icmp(IntCond::Ugt, end, top);
    b.cond_br(over, (l_full, &[]), (l_store, &[]));
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use gravity_ir::{ConvOp, Type};

    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
        fn munmap(addr: *mut u8, len: usize) -> i32;
        fn getpagesize() -> i32;
    }

    /// D3661: a fixed-width image (6, 12, 16 bytes) whose last byte is a mapping's last byte (the next page PROT_NONE):
    /// `store_image` reads no byte past it (that read is a SIGSEGV); the row holds the image, its last word's bytes above
    /// it zero.
    #[test]
    fn a_fixed_image_ending_its_mapping_is_stored_without_reading_past_it() {
        // SAFETY: a private anonymous two-page mapping, the second page made inaccessible; unmapped below.
        let (base, page) = unsafe {
            let page = getpagesize() as usize;
            let map_anon = if cfg!(target_os = "macos") { 0x1000 } else { 0x20 };
            let base = mmap(core::ptr::null_mut(), 2 * page, 3, 0x02 | map_anon, -1, 0);
            assert!(!base.is_null() && base as isize != -1, "mmap");
            assert_eq!(mprotect(base.add(page), page, 0), 0, "mprotect");
            (base, page)
        };
        for typlen in [6i16, 12, 16] {
            let img: Vec<u8> = (1..=typlen as u8).collect();
            // SAFETY: the image's bytes end at the first page's last byte.
            let src = unsafe {
                let at = base.add(page - img.len());
                core::ptr::copy_nonoverlapping(img.as_ptr(), at, img.len());
                at as i64
            };
            let mut words = [u64::from_ne_bytes([0xEE; 8]); 4];
            let dst = words.as_mut_ptr() as i64;
            crate::frame::stop::tests::run1(&[], &mut |b, env| {
                let at = b.iconst(Type::I64, dst);
                let at = b.conv(ConvOp::IntToPtr, Type::Ptr, at);
                let ptr = b.iconst(Type::I64, src);
                let valid = b.bconst(true);
                super::store_image(b, env, at, 8, typlen, ptr, valid);
                env.zero
            });
            let row: Vec<u8> = words.iter().flat_map(|w| w.to_ne_bytes()).collect();
            let size = img.len().next_multiple_of(8);
            assert_eq!(&row[8..8 + img.len()], &img[..], "typlen {typlen}: the image");
            assert!(row[8 + img.len()..8 + size].iter().all(|&b| b == 0), "typlen {typlen}: the bytes above it are zero");
            assert!(row[..8].iter().chain(&row[8 + size..]).all(|&b| b == 0xEE), "typlen {typlen}: nothing else written");
        }
        // SAFETY: the mapping made above; no image outlives it.
        assert_eq!(unsafe { munmap(base, 2 * page) }, 0);
    }
}
