use gravity_abi::PtrWord;
use crate::host_abi::*;

// ---- the per-code tables' entries (lowerv2 text/codes.rs, text/render.rs): the per-part bind and the dictionary render ----

/// A bind class: the table form the IR reads (`TableRef::{Bits, Len | LenMasked, Fp128, Words}`).
pub const BIND_BITS: u8 = 0;
pub const BIND_LEN: u8 = 1;
pub const BIND_FP: u8 = 2;
pub const BIND_WORDS: u8 = 3;
/// The out words per bound table: the data pointer, the fault words (a zero page when it has none), the length form.
pub const BIND_OUT_WORDS: usize = 3;
pub const BIND_FORM_WORDS: u64 = 0;
pub const BIND_FORM_IDX_BYTES: u64 = 1;
pub const BIND_FORM_IDX_DELTA: u64 = 2;
pub const BIND_FORM_IDX_CHARS: u64 = 3;

/// D1747: the flags words after the `n` tables' out words (`out[3n..]`): bit 0 = no table has faults, bit `1 + i` = table `i`
/// unfilled, as many words as the tables need (no cap on a site's tables).
pub const fn bind_flag_words(n: usize) -> usize {
    (n + 1).div_ceil(64)
}

/// The `BIND_OUT_WORDS` slots the flags words take (a site's row tables' slots follow them).
pub const fn bind_flag_slots(n: usize) -> usize {
    bind_flag_words(n).div_ceil(BIND_OUT_WORDS)
}

/// The fault words of a 24-bit code space (2 MiB).
const ZERO_FAULT_WORDS: usize = 1 << (gravity_abi::status::FAULT_CODE_BITS - 6);

/// The fault bitmap `agg_bind` hands out for a table with no faults: every code's bit reads 0, so a kernel reads the bit
/// without a branch. Interior-mutable only so that LLVM places it in `.bss` (zero file bytes, untouched pages are the
/// kernel's zero page); an immutable all-zero static stays in `.rodata` as 2 MiB of file. Nothing ever writes it: a
/// program writes fault bits only into an unfilled table's own bitmap (`percode_bind.rs` `sweep`, `percode_fill.rs`
/// `faulting`), which the first bind hands out, never this one.
#[repr(transparent)]
struct ZeroFaults(core::cell::UnsafeCell<[u64; ZERO_FAULT_WORDS]>);

// SAFETY: never written (the only access is `ptr`, read through by bound kernels), so shared reads from any thread are sound.
unsafe impl Sync for ZeroFaults {}

impl ZeroFaults {
    const fn ptr(&self) -> *const u64 {
        self.0.get() as *const u64
    }
}

static ZERO_FAULTS: ZeroFaults = ZeroFaults(core::cell::UnsafeCell::new([0; ZERO_FAULT_WORDS]));

/// The fault bitmap of a table with no faults: a bound table whose fault words are these has none to read.
pub(crate) fn zero_faults() -> *const u64 {
    ZERO_FAULTS.ptr()
}

crate::contract::host! {
    /// Once per window: the site's table lanes named by `spec[i] = lane | class << 16` filled for the window's part (`lane_fill`) and read
    /// back as `out[3i..]` = data, faults (the zero page when none), the length form; `out[3n..]` (`bind_flag_words`) bit 0 = no table
    /// has faults, bit `1 + i` = table `i` is a `PROGRAM` table the source allocated for the program's sweep (`agg_table_done`, then a
    /// re-bind). A lane whose
    /// published element is not the class's form, or a length / fingerprint table with faults, leaves `UNSUPPORTED_SHAPE`.
    pub unsafe fn agg_bind(ctx: u64, site: u64, spec: u64, n: u64, out: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        let spec = spec as *const u64;
        let out = out as *mut u64;
        guard!(ERROR, {
            use gravity_abi::site::{lane_fill, table_lane};
            use gravity_dict::lane::{elem, is_table, ELEM_BITS, ELEM_FP128, ELEM_I64, ELEM_LEN_IDX, ELEM_LEN_IDX_DELTA};
            use gravity_dict::table::{Header, LEN_MODE_CHARS_ABSOLUTE, LEN_MODE_CHARS_DELTA};
            let s = gravity_abi::ctx::site_ptr(ctx, site as usize);
            let spec = core::slice::from_raw_parts(spec, n as usize);
            let mut all_clean = 1u64;
            let flags = out.add(BIND_OUT_WORDS * n as usize);
            for w in 0..bind_flag_words(n as usize) {
                *flags.add(w) = 0;
            }
            for (i, &w) in spec.iter().enumerate() {
                let (k, class) = ((w & 0xffff) as u32, (w >> 16) as u8);
                let st = lane_fill(s, k);
                if st < 0 {
                    return st;
                }
                let l = table_lane(s, k as usize);
                if !is_table(l) {
                    return unsupported(UNSUPPORTED_SHAPE);
                }
                let hdr = l.ptr.ptr() as *const Header;
                if st == gravity_abi::site::LANE_UNFILLED {
                    // the program sweeps it before the re-bind: the data and the fault bitmap it writes, the class check deferred
                    *flags.add((1 + i) / 64) |= 1 << ((1 + i) % 64);
                    all_clean = 0;
                    *out.add(BIND_OUT_WORDS * i) = (*hdr).data.ptr() as u64;
                    *out.add(BIND_OUT_WORDS * i + 1) = (*hdr).faults.ptr() as u64;
                    *out.add(BIND_OUT_WORDS * i + 2) = BIND_FORM_WORDS;
                    continue;
                }
                let form = match (class, elem(l)) {
                    (BIND_BITS, ELEM_BITS) | (BIND_WORDS, ELEM_I64) | (BIND_LEN, ELEM_I64) | (BIND_FP, ELEM_FP128) => BIND_FORM_WORDS,
                    (BIND_LEN, ELEM_LEN_IDX) => match (*hdr).pad[0] {
                        LEN_MODE_CHARS_ABSOLUTE => BIND_FORM_IDX_CHARS,
                        LEN_MODE_CHARS_DELTA => BIND_FORM_IDX_DELTA,
                        _ => BIND_FORM_IDX_BYTES,
                    },
                    (BIND_LEN, ELEM_LEN_IDX_DELTA) => BIND_FORM_IDX_DELTA,
                    _ => return unsupported(UNSUPPORTED_SHAPE),
                };
                let faults = (*hdr).faults.ptr();
                if !faults.is_null() && matches!(class, BIND_LEN | BIND_FP) {
                    return unsupported(UNSUPPORTED_SHAPE);
                }
                *out.add(BIND_OUT_WORDS * i) = (*hdr).data.ptr() as u64;
                *out.add(BIND_OUT_WORDS * i + 1) = if faults.is_null() {
                    ZERO_FAULTS.ptr() as u64
                } else {
                    all_clean = 0;
                    faults as u64
                };
                *out.add(BIND_OUT_WORDS * i + 2) = form;
            }
            *flags |= all_clean;
            0
        })
    }
}

crate::contract::host! {
    /// The code count of table lane `k` of `site` (the program's sweep of an unfilled `PROGRAM` table runs over `0..ncodes`).
    pub unsafe fn agg_table_ncodes(ctx: u64, site: u64, k: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        guard!(ERROR, {
            let s = gravity_abi::ctx::site_ptr(ctx, site as usize);
            let l = gravity_abi::site::table_lane(s, k as usize);
            if !gravity_dict::lane::is_table(l) {
                return bare("an aggregate's table lane is not a dictionary table");
            }
            (*(l.ptr.ptr() as *const gravity_dict::table::Header)).ncodes as i64
        })
    }
}

crate::contract::host! {
    /// D3115: the first code of the next `SWEEP_CHUNK` codes of unfilled table lane `k` this source sweeps (every source of
    /// the statement that reached the part shares the sweep), or the table's code count once every chunk is swept;
    /// `CANCELLED` when the cancel byte is set while another source still sweeps; a failure status below zero.
    pub unsafe fn agg_table_claim(ctx: u64, site: u64, k: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        guard!(ERROR, {
            let s = gravity_abi::ctx::site_ptr(ctx, site as usize);
            loop {
                let r = gravity_abi::site::lane_fill(s, k as u32 | gravity_abi::site::LANE_CLAIM);
                if r != gravity_abi::site::CLAIM_WAIT {
                    return r;
                }
                if gravity_abi::ctx::cancel_pending(ctx) {
                    return gravity_abi::status::CANCELLED;
                }
            }
        })
    }
}

crate::contract::host! {
    /// The program's sweep of table lane `k` is done with `nfaults` fault bits set (none: the bitmap is dropped, so the re-bind reads a
    /// clean table); the source publishes the table for the part (`LANE_DONE`).
    pub unsafe fn agg_table_done(ctx: u64, site: u64, k: u64, nfaults: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        guard!(ERROR, {
            use gravity_dict::table::{Header, FLAG_FAULTS};
            let s = gravity_abi::ctx::site_ptr(ctx, site as usize);
            let l = gravity_abi::site::table_lane(s, k as usize);
            if !gravity_dict::lane::is_table(l) {
                return bare("an aggregate's table lane is not a dictionary table");
            }
            let hdr = l.ptr.ptr() as *mut Header;
            if nfaults == 0 {
                (*hdr).faults = gravity_abi::word(core::ptr::null());
                (*hdr).flags &= !FLAG_FAULTS;
            } else {
                (*hdr).nfaults = nfaults as u32;
            }
            let done = gravity_abi::site::lane_fill(s, k as u32 | gravity_abi::site::LANE_DONE);
            if done < 0 {
                return done;
            }
            0
        })
    }
}

/// D2074: a row filled one faulting entry of lazy `PROGRAM` table lane `k` (lowerv4 dict percode_fill.rs, the table published
/// with every entry faulting; `contract::entry2::<TableFilled>`: `(site, k)`): its fault count drops, and the last one drops
/// the bitmap, so the next bind reads the table clean as a swept table with no faults. The bitmap's memory stays for a window
/// still reading it (every bit clear by then). Answers 0, or the failure status (below zero as an `i64`).
pub struct TableFilled;

impl crate::contract::Host for TableFilled {
    type Args = (u64, u64);
    /// # Safety
    /// `ctx` is the running program's bound context and `site` its dictionary aggregate site with table lane `k`.
    unsafe fn run(ctx: *mut Ctx, (site, k): (u64, u64)) -> u64 {
        use gravity_dict::table::{Header, FLAG_FAULTS};
        let s = gravity_abi::ctx::site_ptr(ctx, site as usize);
        let l = gravity_abi::site::table_lane(s, k as usize);
        if !gravity_dict::lane::is_table(l) {
            return bare("an aggregate's table lane is not a dictionary table") as u64;
        }
        let hdr = l.ptr.ptr() as *mut Header;
        if (*hdr).nfaults > 0 {
            (*hdr).nfaults -= 1;
            if (*hdr).nfaults == 0 {
                (*hdr).faults = gravity_abi::word(core::ptr::null());
                (*hdr).flags &= !FLAG_FAULTS;
            }
        }
        0
    }
}

/// The emit's render of a code word `part << 32 | code` of code lane `lane` through the site's dictionary: the varlena image's
/// address (the part's resident payload, no copy), null for `MINCODE_NONE` (the SQL NULL), or the failed dictionary read's
/// status (below zero as an `i64`; its error stashed in the node) that the caller raises at once, never a NULL (D344).
pub const AGG_RENDER_LIFE: gravity_fn::ResultLife = gravity_fn::ResultLife::IntoArgument(3);
crate::contract::host! {
    pub unsafe fn agg_render(ctx: u64, site: u64, lane: u64, word: u64) -> u64 {
        // the host ABI takes and answers every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        guard!(ERROR as *const u8, {
            match crate::dict::reps::render(gravity_abi::ctx::site_ptr(ctx, site as usize), lane as u8, word) {
                Ok(p) => p.unwrap_or(core::ptr::null()),
                Err(st) => st as usize as *const u8,
            }
        }) as u64
    }
}

#[cfg(test)]
mod tests {
    /// The bitmap's first and last word, read through its address as a bound kernel reads it (base and tip alike).
    fn words() -> (*const u64, usize) {
        (core::ptr::addr_of!(super::ZERO_FAULTS) as *const u64, 1 << (gravity_abi::status::FAULT_CODE_BITS - 6))
    }

    #[test]
    fn zero_faults_reads_zero_over_every_code() {
        let (p, n) = words();
        assert_eq!(n * 64, 1 << gravity_abi::status::FAULT_CODE_BITS);
        // SAFETY: `p` covers `n` words of a static.
        unsafe {
            assert_eq!(core::ptr::read_volatile(p), 0);
            assert_eq!(core::ptr::read_volatile(p.add(n / 2)), 0);
            assert_eq!(core::ptr::read_volatile(p.add(n - 1)), 0);
        }
    }

    /// D4500: the bitmap lies in a writable (`.bss`) mapping, not the read-only one `.rodata` maps into, so it costs no
    /// file bytes. Born red on trunk (the immutable static lands in `.rodata`: an `r--p` / `r-xp` mapping).
    #[cfg(target_os = "linux")]
    #[test]
    fn zero_faults_is_bss_not_rodata() {
        let (p, n) = words();
        let (lo, hi) = (p as usize, p as usize + n * 8 - 1);
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        for at in [lo, hi] {
            let line = maps
                .lines()
                .find(|l| {
                    let (a, b) = l.split(' ').next().unwrap().split_once('-').unwrap();
                    (usize::from_str_radix(a, 16).unwrap()..usize::from_str_radix(b, 16).unwrap()).contains(&at)
                })
                .unwrap_or_else(|| panic!("no mapping holds {at:#x}"));
            let perms = line.split(' ').nth(1).unwrap();
            assert!(perms.starts_with("rw"), "the zero fault bitmap at {at:#x} is in a read-only mapping (.rodata): {line}");
        }
    }
}
