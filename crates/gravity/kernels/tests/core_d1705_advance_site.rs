//! D1705 (Miri, Tree Borrows): the source behind `Site.next` rewrites the site block in place, so a `&mut Site` a kernel
//! helper holds across `window` is disabled by that write. `util::advance` and `code.heads`' count read the block after
//! the window through a reference taken before it: plain cargo passes, Miri TB stops at the read.
use gravity_abi::ctx::{ctx_size, lane, site_off, site_ptr};
use gravity_abi::site::site::{STATE_EOF, STATE_OPEN};
use gravity_abi::{Ctx, Lane, Site};
use gravity_kernels::dict::strategies::heads::heads_count;
use gravity_kernels::util::advance;

const ROWS: u32 = 4;
const CODES: [u32; ROWS as usize] = [7, 9, 7, 7];

/// The source state behind `Site.src`: windows left, windows served.
struct Src {
    left: u32,
    served: u32,
}

/// A window source as the driver's (`Window::over` + `set`): the block rewritten through the pointer the host was handed,
/// `ROWS` rows of a 4-byte code lane in part 3 per window, 0 once `left` runs out.
unsafe extern "C" fn next(s: *mut Site) -> i64 {
    let st = &mut *s;
    let src = &mut *(st.src as *mut Src);
    st.state = STATE_OPEN;
    (st.cursor, st.sel, st.sel_n, st.nlanes) = (0, core::ptr::null(), 0, 0);
    if src.left == 0 {
        st.rows = 0;
        return 0;
    }
    src.left -= 1;
    src.served += 1;
    *st.lanes = Lane::byval(CODES.as_ptr() as *const u8, core::ptr::null(), 4, lane::REGION_ARENA, 0);
    (st.rows, st.nlanes, st.part_id) = (ROWS, 1, 3);
    ROWS as i64
}

/// A one-site ctx in `buf` laid out as the driver binds it (the site inline behind the header) over `src`.
/// # Safety — every argument outlives the ctx's use.
unsafe fn bind(buf: &mut [u64], src: &mut Src, lanes: &mut [Lane; 1], keys: &mut [u64; 2], keys_null: &mut [u8; 2]) -> *mut Ctx {
    assert!(buf.len() * 8 >= ctx_size(1));
    let ctx = buf.as_mut_ptr() as *mut Ctx;
    (*ctx).sites = (ctx as *mut u8).add(site_off(0)) as *mut Site;
    (*ctx).nsites = 1;
    let s = site_ptr(ctx, 0);
    (*s).next = Some(next);
    (*s).src = src as *mut Src as *mut u8;
    (*s).lanes = lanes.as_mut_ptr();
    (*s).lanes_cap = 1;
    (*s).keys = keys.as_mut_ptr();
    (*s).keys_null = keys_null.as_mut_ptr();
    (*s).nkeys = 2;
    ctx
}

/// `util::advance` over three windows: each window's rows, then 0 with the site at `EOF`. The parent held `util::site`'s
/// `&mut Site` across `window`, so its read of `cursor` after the first window went through a disabled tag.
#[test]
fn advance_reads_the_block_the_window_rewrote() {
    let mut buf = vec![0u64; ctx_size(1) / 8];
    let mut src = Src { left: 3, served: 0 };
    let (mut lanes, mut keys, mut keys_null) = ([Lane::NONE; 1], [0u64; 2], [0u8; 2]);
    let mut got = Vec::new();
    // SAFETY: the ctx is bound over locals that outlive it; a window's rows are taken by moving the cursor, as a kernel does.
    unsafe {
        let ctx = bind(&mut buf, &mut src, &mut lanes, &mut keys, &mut keys_null);
        loop {
            let n = advance(ctx, 0);
            assert!(n >= 0, "advance failed: {n}");
            if n == 0 {
                break;
            }
            got.push(n);
            (*site_ptr(ctx, 0)).cursor += n as u32;
        }
        assert_eq!((*site_ptr(ctx, 0)).state, STATE_EOF);
    }
    assert_eq!(got, [ROWS as i64; 3]);
    assert_eq!(src.served, 3);
}

/// `code.heads`' count (V4R-310, a product host) over the same source: code 7's rows in part 3 across three windows. The
/// parent read `part_id`, `lanes` and `cursor` after `advance` through the reference it took before the seek.
#[test]
fn heads_count_reads_each_window_after_the_host_rewrote_it() {
    let mut buf = vec![0u64; ctx_size(1) / 8];
    let mut src = Src { left: 3, served: 0 };
    let (mut lanes, mut keys, mut keys_null) = ([Lane::NONE; 1], [0u64; 2], [0u8; 2]);
    // SAFETY: the ctx is bound over locals that outlive it; site 0 has two key cells.
    let c = unsafe { heads_count((bind(&mut buf, &mut src, &mut lanes, &mut keys, &mut keys_null)) as u64, 0, 5 << 32 | 7, 0, 0) };
    assert_eq!(c, 9, "three windows of [7, 9, 7, 7]");
    assert_eq!((keys, keys_null), ([5, u64::MAX], [0, 0]), "the part's first granule as the seek keys");
    assert_eq!(src.served, 3);
}
