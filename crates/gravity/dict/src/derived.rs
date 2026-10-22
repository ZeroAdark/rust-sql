//! A derived dictionary: `f` maps each entry to a new byte string (a `regexp_replace`, a
//! `substring`, a `lower`), stored once per code with its hash, and codes whose derived values
//! collide inside the part are mapped to one canonical index (§3 case 9).
use gravity_abi::{word, PtrWord};
use crate::table::{bit, keep_word, Build, CodeCell, Deferred, Halt, Header};
use crate::{BuildError, Dict, Region, View};
use gravity_abi::hash::hash_bytes64;

#[derive(Clone, Copy, Debug)]
pub struct Derived {
    hdr: *const Header,
    ents: *const u32,
    base: *const u8,
    canon: *const u32,
    hashes: *const u64,
    faults: *const u64,
    ncodes: u32,
    ndistinct: u32,
}

impl Derived {
    /// `f(entry, region)` returns the derived bytes: a view into the region (the shim wrote them
    /// there, kept in place) or anywhere else (copied). The dedup map is region scratch popped
    /// before the build returns.
    pub fn build(dict: &dyn Dict, region: &mut Region, f: &mut dyn FnMut(View, &mut Region) -> Result<View, Halt>) -> Result<Derived, BuildError> {
        Derived::build_in(dict, region, None, f)
    }

    /// `build` with every derived value a subslice of `over` (a part's resident payload, live as
    /// long as the table): the values are kept in place and the table holds offsets into it —
    /// nothing copied. `f` must return views inside `over`.
    pub fn build_over(dict: &dyn Dict, region: &mut Region, over: &[u8], f: &mut dyn FnMut(View, &mut Region) -> Result<View, Halt>) -> Result<Derived, BuildError> {
        Derived::build_in(dict, region, Some(over), f)
    }

    fn build_in(dict: &dyn Dict, region: &mut Region, over: Option<&[u8]>, f: &mut dyn FnMut(View, &mut Region) -> Result<View, Halt>) -> Result<Derived, BuildError> {
        let mut b = Build::start(dict, region, crate::lane::ELEM_DERIVED)?;
        let n = b.ncodes() as usize;
        let ents = region.alloc_zeroed(n * 8, 8)? as *mut u32;
        let hashes = region.alloc_zeroed(n * 8, 8)? as *mut u64;
        let canon = region.alloc_zeroed(n * 4, 8)? as *mut u32;
        // the run hint, parked in `canon` until the dedup assigns it: the code before this one in
        // the sweep (plus one; 0 none) when the two derived values are the same bytes — entries come
        // in byte order, so a host's entries run together: no hash, no probe for the run
        let mut prev: Option<(u32, View, u64)> = None;
        let base = over.map_or(region.base(), <[u8]>::as_ptr);
        let inside = |d: View, region: &Region| match over {
            Some(o) => (o.as_ptr() as usize..=o.as_ptr() as usize + o.len()).contains(&(d.ptr as usize)) && d.ptr as usize + d.len as usize <= o.as_ptr() as usize + o.len(),
            None => region.contains(d.ptr),
        };
        crate::table::sweep(dict, region, &mut b, &mut |code, v, region| {
            let d = f(v, region)?;
            // SAFETY: `d` is live; `code < ncodes`; the arrays hold `ncodes` cells.
            unsafe {
                let d = if inside(d, region) {
                    d
                } else if d.len == 0 {
                    View { ptr: base, len: 0 }
                } else if over.is_some() {
                    return Err(Halt::Region(BuildError::OutsideBase));
                } else {
                    region.copy(d.bytes())?
                };
                *ents.add(2 * code as usize) = (d.ptr as usize - base as usize) as u32;
                *ents.add(2 * code as usize + 1) = d.len;
                let h = match prev {
                    Some((pc, pv, ph)) if pv.len == d.len && pv.bytes() == d.bytes() => {
                        *canon.add(code as usize) = pc + 1;
                        ph
                    }
                    _ => hash_bytes64(d.bytes()),
                };
                *hashes.add(code as usize) = h;
                prev = Some((code, d, h));
            }
            Ok(())
        })?;
        let mark = region.mark();
        let slots = (2 * n).next_power_of_two().max(2);
        let map = region.alloc_zeroed(slots * 4, 8)? as *mut u32;
        let mut ndistinct = 0u32;
        let faults = b.faults_ptr();
        let null = dict.null_code();
        for code in 0..n as u32 {
            // SAFETY: the arrays hold `ncodes` cells; the map has `slots` cells; views lie in the region.
            unsafe {
                if Some(code) == null || (!faults.is_null() && bit(faults, code)) {
                    *canon.add(code as usize) = u32::MAX;
                    continue;
                }
                let hc = *canon.add(code as usize);
                if hc != 0 && hc - 1 < code {
                    *canon.add(code as usize) = *canon.add(hc as usize - 1);
                    continue;
                }
                *canon.add(code as usize) = 0;
                let h = *hashes.add(code as usize);
                let mut i = (h as usize) & (slots - 1);
                loop {
                    let slot = *map.add(i);
                    if slot == 0 {
                        *map.add(i) = code + 1;
                        *canon.add(code as usize) = ndistinct;
                        ndistinct += 1;
                        break;
                    }
                    let other = slot - 1;
                    if *hashes.add(other as usize) == h && view_at(ents, base, other).bytes() == view_at(ents, base, code).bytes() {
                        *canon.add(code as usize) = *canon.add(other as usize);
                        break;
                    }
                    i = (i + 1) & (slots - 1);
                }
            }
        }
        region.reset_to(mark);
        let hdr = b.finish(ents as *const u8);
        // SAFETY: the header this build wrote.
        unsafe {
            let h = hdr as *mut Header;
            (*h).base = word(base);
            (*h).canon = word(canon);
            (*h).hashes = word(hashes);
            (*h).ndistinct = ndistinct;
            Ok(Derived::at(hdr))
        }
    }

    /// # Safety — `hdr` is a `Derived` table header.
    pub unsafe fn at(hdr: *const Header) -> Derived {
        debug_assert_eq!((*hdr).elem, crate::lane::ELEM_DERIVED);
        Derived { hdr, ents: (*hdr).data.ptr() as *const u32, base: (*hdr).base.ptr(), canon: (*hdr).canon.ptr(), hashes: (*hdr).hashes.ptr(), faults: (*hdr).faults.ptr(), ncodes: (*hdr).ncodes, ndistinct: (*hdr).ndistinct }
    }

    pub fn header(&self) -> *const Header {
        self.hdr
    }

    pub fn ncodes(&self) -> u32 {
        self.ncodes
    }

    /// Distinct derived values among the non-faulting, non-null codes.
    pub fn ndistinct(&self) -> u32 {
        self.ndistinct
    }

    pub fn has_faults(&self) -> bool {
        !self.faults.is_null()
    }

    /// # Safety — `code < ncodes()`.
    #[inline(always)]
    pub unsafe fn view(&self, code: u32) -> View {
        view_at(self.ents, self.base, code)
    }

    /// The canonical index of `code`'s derived value in this part (`u32::MAX` for a faulting or null code).
    /// # Safety — `code < ncodes()`.
    #[inline(always)]
    pub unsafe fn canonical(&self, code: u32) -> u32 {
        *self.canon.add(code as usize)
    }

    /// # Safety — `code < ncodes()`.
    #[inline(always)]
    pub unsafe fn hash(&self, code: u32) -> u64 {
        *self.hashes.add(code as usize)
    }

    /// # Safety — `code < ncodes()`.
    #[inline(always)]
    pub unsafe fn faulted(&self, code: u32) -> bool {
        !self.faults.is_null() && bit(self.faults, code)
    }

    /// `out[i]` ← canonical(codes[i]) for the valid rows; the first valid faulting row is returned.
    /// # Safety — as `I64::gather`, `out` holds `rows` cells.
    pub unsafe fn gather_canonical<C: CodeCell>(&self, codes: *const C, valid: *const u64, rows: usize, out: *mut u32) -> Result<(), Deferred> {
        for i in 0..rows {
            if valid.is_null() || bit(valid, i as u32) {
                *out.add(i) = *self.canon.add((*codes.add(i)).code() as usize);
            }
        }
        if self.faults.is_null() {
            return Ok(());
        }
        for w in 0..rows.div_ceil(64) {
            let keep = keep_word(valid, rows, true, &[], w);
            let mut m = keep;
            while m != 0 {
                let i = w * 64 + m.trailing_zeros() as usize;
                let c = (*codes.add(i)).code();
                if bit(self.faults, c) {
                    return Err(Deferred { row: i as u32, code: c });
                }
                m &= m - 1;
            }
        }
        Ok(())
    }
}

#[inline(always)]
unsafe fn view_at(ents: *const u32, base: *const u8, code: u32) -> View {
    View { ptr: base.add(*ents.add(2 * code as usize) as usize), len: *ents.add(2 * code as usize + 1) }
}
