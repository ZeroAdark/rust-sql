//! append's raw half: the pull-form merge's hosts, entered through the contract's landing pad (kernels contract.rs
//! `entry3`): the comparator of a comparator-ordered key (merge_pull_cmp.rs `less`) and the copy of a fixed-width
//! by-reference column (merge_pull_copy.rs `copy_fixed`); and its two direct hosts (rules R6 / R8), written through the
//! contract's `host!` (no marker: `extern "C"`, the body as written), the text keys' `varstr_cmp` over two held copies
//! (merge_pull_cmp.rs `less`) and a varlena column's held copy (merge_pull_copy.rs `copy`). Owner append.
#![allow(unsafe_code)]
use gravity_abi::ctx::Ctx;

/// `proc(a, x)` under `coll` over two held copies (detoasted Datums behind a 4-byte header, merge_pull_copy.rs), the third
/// word `proc | coll << 32` (`proc` a btree support function's resolution id, runs/keylane.rs `cmp_proc`): −1 / 0 / 1, or
/// 2 when the bind or the call raised (its error stashed), through KeySem's bound-call table (`keysem::call::order`), as
/// the IR reads it.
pub(crate) struct PullKeyHosted;

impl gravity_kernels::contract::Host for PullKeyHosted {
    type Args = (u64, u64, u64);

    unsafe fn run(_ctx: *mut gravity_abi::Ctx, (a, x, pc): (u64, u64, u64)) -> u64 {
        match gravity_fn::typesem::keysem::call::order(pc as u32, (pc >> 32) as u32, a, x) {
            Ok(o) => o as i8 as i64 as u64,
            Err(e) => {
                gravity_fn::stash_error(e);
                2
            }
        }
    }

    /// A panic in the comparator reads as raised.
    fn fail(_a: (u64, u64, u64)) -> u64 {
        2
    }
}

/// The `len`-byte image at `datum` (0: NULL, 0 back) copied into the buffer at `buf` (frame words `[ptr, cap]`), grown to
/// the larger of `len` and twice the old capacity (64 bytes at least) by a statement pin, as merge_pull_copy.rs `pull_copy`
/// grows a varlena's: its address back, or -1 when the pin is refused (its error stashed).
pub(crate) struct PullCopyFixed;

impl gravity_kernels::contract::Host for PullCopyFixed {
    type Args = (u64, u64, u64);

    unsafe fn run(ctx: *mut gravity_abi::Ctx, (buf, datum, len): (u64, u64, u64)) -> u64 {
        if datum == 0 {
            return 0;
        }
        let (ptr, cap) = (buf as *mut u64, (buf as *mut u64).add(1));
        if *cap < len {
            let size = len.max(2 * *cap).max(64);
            let p = gravity_abi::program::pin(ctx, size);
            if p.is_null() {
                gravity_kernels::host_abi::pin_refused(size as usize); // D1392 (b)
                return u64::MAX;
            }
            (*ptr, *cap) = (p as u64, size);
        }
        core::ptr::copy_nonoverlapping(datum as usize as *const u8, *ptr as usize as *mut u8, len as usize);
        *ptr
    }

    /// A panic reads as a failed copy (the IR raises).
    fn fail(_a: (u64, u64, u64)) -> u64 {
        u64::MAX
    }
}

gravity_kernels::contract::host! {
    /// `varstr_cmp` of the two held copies `a` and `x` (4-byte-header varlenas, merge_pull_copy.rs) under collation `coll`:
    /// −1 / 0 / 1, or 2 when the compare raised (its error stashed). # Safety — `a` and `x` are live held copies.
    pub(crate) unsafe fn pull_key_cmp(a: u64, x: u64, coll: u64) -> i64 {
        let body = |p: u64| {
            let p = p as usize as *const u8;
            let len = ((p as *const u32).read_unaligned() >> 2) as usize - 4;
            (p.add(4), len as u64)
        };
        let ((pa, la), (px, lx)) = (body(a), body(x));
        gravity_kernels::host_abi::text_cmp_coll(pa, la, px, lx, coll)
    }

    /// The copy of the varlena at `datum` (0: NULL, 0 back) into the buffer at `buf` (`[ptr, cap]`), grown to the larger of
    /// the value and twice the old capacity (64 bytes at least) by a statement pin. Below zero: a failed detoast or pin, its
    /// error stashed. # Safety — `datum` is 0 or a live varlena; `buf` two frame words.
    pub(crate) unsafe fn pull_copy(ctx: u64, buf: u64, datum: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        let buf = buf as *mut u64;
        if datum == 0 {
            return 0;
        }
        let copy = gravity_fn::with_payload(datum as usize as *const u8, |x| {
            let need = 4 + x.len();
            let (ptr, cap) = (buf, buf.add(1));
            if (*cap as usize) < need {
                let size = need.max(2 * *cap as usize).max(64);
                let p = gravity_abi::program::pin(ctx, size as u64);
                if p.is_null() {
                    gravity_kernels::host_abi::pin_refused(size); // D1392 (b)
                    return -1;
                }
                (*ptr, *cap) = (p as u64, size as u64);
            }
            let d = *ptr as *mut u8;
            (d as *mut u32).write_unaligned((need as u32) << 2);
            core::ptr::copy_nonoverlapping(x.as_ptr(), d.add(4), x.len());
            d as i64
        });
        copy.unwrap_or_else(|e| {
            gravity_fn::stash_error(e);
            -1
        })
    }
}

#[cfg(test)]
mod tests {
    use super::pull_key_cmp;

    fn held(s: &[u8]) -> Vec<u64> {
        let mut v = vec![0u64; (4 + s.len()).div_ceil(8)];
        let p = v.as_mut_ptr() as *mut u8;
        // SAFETY: `v` holds 4 + s.len() bytes.
        unsafe {
            (p as *mut u32).write_unaligned(((4 + s.len()) as u32) << 2);
            core::ptr::copy_nonoverlapping(s.as_ptr(), p.add(4), s.len());
        }
        v
    }

    /// Two held copies compare by their payloads under C (950): a shared 8-byte prefix is decided past it, a prefix
    /// orders first, equal payloads tie.
    #[test]
    fn held_copies_compare_past_their_prefix_word() {
        let cmp = |a: &[u8], x: &[u8]| {
            let (ha, hx) = (held(a), held(x));
            // SAFETY: two live held copies.
            unsafe { pull_key_cmp(ha.as_ptr() as u64, hx.as_ptr() as u64, 950) }
        };
        assert_eq!(cmp(b"abcdefgh-1", b"abcdefgh-2"), -1);
        assert_eq!(cmp(b"abcdefgh-2", b"abcdefgh-1"), 1);
        assert_eq!(cmp(b"abcdefgh", b"abcdefgh-1"), -1);
        assert_eq!(cmp(b"abcdefgh-1", b"abcdefgh-1"), 0);
        assert_eq!(cmp(b"", b""), 0);
    }
}

#[cfg(test)]
mod d1392_tests {
    use super::*;

    /// D1392 (b): a ctx whose program binds no pin (`gravity_abi::program::pin` answers null, as a refused pool does).
    fn refused_pin_ctx() -> (Vec<u64>, Box<gravity_abi::ProgramBlock>) {
        let mut program = Box::new(gravity_abi::ProgramBlock::NONE);
        let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(1) / 8];
        // SAFETY: the words are a ctx image; `program` outlives every use.
        unsafe { (*(ctx.as_mut_ptr() as *mut Ctx)).program = &mut *program };
        (ctx, program)
    }

    /// The stashed error is PostgreSQL's "out of memory" (ERRCODE_OUT_OF_MEMORY), never nothing (the client's "an error
    /// without a message (raise code 7)").
    fn assert_out_of_memory() {
        let e = gravity_fn::take_error().expect("a refused pin stashes its error for the HOST raise");
        assert_eq!((e.message(), e.sqlstate()), ("out of memory", types_error::ERRCODE_OUT_OF_MEMORY));
        assert!(e.detail().is_some_and(|d| d.contains("pinned region")), "{:?}", e.detail());
    }

    /// A 4-byte-header varlena of `s`, 8-aligned.
    fn varlena(s: &[u8]) -> Vec<u64> {
        let mut v = vec![0u64; (4 + s.len()).div_ceil(8)];
        let p = v.as_mut_ptr() as *mut u8;
        // SAFETY: `v` holds 4 + s.len() bytes.
        unsafe {
            (p as *mut u32).write_unaligned(((4 + s.len()) as u32) << 2);
            core::ptr::copy_nonoverlapping(s.as_ptr(), p.add(4), s.len());
        }
        v
    }

    #[test]
    fn a_pulled_copy_the_pool_refuses_names_out_of_memory() {
        let (mut ctx, _program) = refused_pin_ctx();
        let (text, mut buf) = (varlena(b"a held arm row's text"), [0u64; 2]);
        drop(gravity_fn::take_error());
        // SAFETY: a bound ctx; `text` a live varlena; `buf` the two buffer words.
        let r = unsafe { pull_copy((ctx.as_mut_ptr() as *mut Ctx) as u64, buf.as_mut_ptr() as u64, text.as_ptr() as u64) };
        assert!(r < 0, "no room: the raise exit");
        assert_out_of_memory();
    }
}
