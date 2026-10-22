//! The leader merge face (lower.md §15.8): the owners' kept records (`OwnerTop`) merged into the query's `n` best under `count DESC, keys ASC`
//! (text by rendered bytes, NULL last) as one sorted run in the arena; a stop's resume finds it through the header in owner 0's table space.
use gravity_abi::{word, Word};
use core::mem::size_of;

use gravity_abi::breaker::{table_kind, KeyKind, Owned, OwnerTop, RecordLayout, MINCODE_NONE, MOVING, ORDER_NONE, TABLE_OWNED};
use gravity_abi::ctx::{arena_alloc, site_ptr, RESUME_NONE};
use gravity_abi::program::partial;
use gravity_abi::status::{unsupported, ERROR, UNSUPPORTED_SCRATCH, UNSUPPORTED_SHAPE};
use gravity_abi::{Ctx, Site};

use crate::dict::reps::{render, varlena_bytes, Reps, MAX_REPS, REP_NONE};
use crate::util::HOST_CONTINUE;

/// The owners' kept copies carry `(part, code)` representatives (the generic owner, `count_topk_text`).
pub const FORM_CODED: u8 = 0;
/// The owners' kept copies carry rendered datums (t3's owner: `Top::offer_lazy`), `0` / `MINCODE_NONE` a NULL key.
pub const FORM_RENDERED: u8 = 1;

/// What the face takes: the owners' record layout, the text keys' representatives, the order and the bound.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaderSpec {
    pub layout: RecordLayout,
    /// `(representative slot, code lane)` per `Fp128` key in key order, `REP_NONE` past the last.
    pub reps: Reps,
    /// The `Count` slot ordered descending first; `ORDER_NONE`: the keys alone.
    pub order_slot: u8,
    /// The scan site the representatives render through.
    pub site: u8,
    pub form: u8,
    /// Bit `t`: text key `t` is unordered by the sort — ties compare fingerprint words; rendered after the merge, run records only (`OwnerDesc::tie_fp`).
    pub tie_fp: u8,
    /// `LIMIT + OFFSET`: the run holds at most this many records.
    pub n: u32,
    /// The walk's OFFSET (at most `n`): the post-merge renders (`render_ties`) start here — OFFSET 1000 at `n` = 1,010 renders 10, not 1,010.
    pub skip: u32,
}

/// The run the face lays: `len` records of `layout.size()` bytes at `base`, sorted, renders done.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaderRun {
    pub base: Word<*mut u8>,
    pub len: u64,
}

#[repr(C)]
struct Header {
    magic: u64,
    n: u64,
    base: u64,
    len: u64,
}

pub const LEADER_MAGIC: u64 = 0x4C45_4144_4552_5231;
pub const HEADER_BYTES: usize = size_of::<Header>();

struct Merge<'a> {
    spec: &'a LeaderSpec,
    site: *mut Site,
    base: *mut u8,
    rec: usize,
    n: usize,
    len: usize,
    worst: usize,
    off_count: usize,
}

/// `a` before `b`; both records in the run's form (rendered representatives).
#[inline(always)]
unsafe fn text_before(pa: u64, pb: u64) -> Option<bool> {
    match (pa, pb) {
        (0, 0) => None,
        (0, _) => Some(false),
        (_, 0) => Some(true),
        _ => {
            let (a, b) = (varlena_bytes(pa as *const u8), varlena_bytes(pb as *const u8));
            if a == b {
                None
            } else {
                Some(a < b)
            }
        }
    }
}

impl Merge<'_> {
    #[inline(always)]
    unsafe fn at(&self, i: usize) -> *mut u8 {
        self.base.add(i * self.rec)
    }

    #[inline(always)]
    unsafe fn count_of(&self, r: *const u8) -> i64 {
        (*(r.add(self.off_count) as *const u64) & !MOVING) as i64
    }

    /// The candidate `r`'s text key `t` as a rendered datum (0: NULL), once into `tmp`; `kept`: `r` is in the run's form.
    #[inline(always)]
    unsafe fn cand_text(&self, r: *const u8, kept: bool, t: usize, off: usize, lane: u8, tmp: &mut [u64; MAX_REPS], done: &mut u32) -> Result<u64, i64> {
        if *done >> t & 1 == 0 {
            let w = *(r.add(off) as *const u64);
            tmp[t] = if kept || self.spec.form == FORM_RENDERED {
                if w == MINCODE_NONE { 0 } else { w }
            } else if w == MINCODE_NONE {
                0
            } else {
                match render(self.site, lane, w) {
                    Ok(Some(p)) => p as u64,
                    _ => return Err(ERROR),
                }
            };
            *done |= 1 << t;
        }
        Ok(tmp[t])
    }

    /// Whether the candidate `r` (an owner's copy, or a kept record when `kept`) ranks before the kept `w`.
    unsafe fn better(&self, r: *const u8, kept: bool, w: *const u8, tmp: &mut [u64; MAX_REPS], done: &mut u32) -> Result<bool, i64> {
        let l = &self.spec.layout;
        if self.spec.order_slot != ORDER_NONE {
            let (ca, cb) = (self.count_of(r), self.count_of(w));
            if ca != cb {
                return Ok(ca > cb);
            }
        }
        let (mut k, mut t) = (0usize, 0usize);
        while k < l.nkeys() {
            let (wa, wb) = (*(r as *const u64).add(k), *(w as *const u64).add(k));
            if l.key_kind(k) == KeyKind::Fp128 {
                let (ha, hb) = (*(r as *const u64).add(k + 1), *(w as *const u64).add(k + 1));
                let (slot, lane) = if t < MAX_REPS { self.spec.reps[t] } else { (REP_NONE, 0) };
                if slot == REP_NONE || self.spec.tie_fp >> t & 1 != 0 {
                    if (wa, ha) != (wb, hb) {
                        return Ok((wa, ha) < (wb, hb));
                    }
                } else {
                    let off = l.slot_off(slot as usize) as usize;
                    let pa = self.cand_text(r, kept, t, off, lane, tmp, done)?;
                    if let Some(b) = text_before(pa, *(w.add(off) as *const u64)) {
                        return Ok(b);
                    }
                }
                k += 2;
                t += 1;
                continue;
            }
            if wa != wb {
                return Ok((wa as i64) < (wb as i64));
            }
            k += 1;
        }
        Ok(false)
    }

    /// `better` over two kept records (every text rendered): the sort's compare.
    unsafe fn before(&self, a: *const u8, b: *const u8) -> bool {
        let (mut tmp, mut done) = ([0u64; MAX_REPS], 0u32);
        self.better(a, true, b, &mut tmp, &mut done).unwrap_or(false)
    }

    /// The kept records as a heap with the worst at the root (`worst` stays 0): a replaced root sifts down in
    /// `log n` compares — a linear scan per kept candidate rendered every kept text key at `n` past a thousand.
    unsafe fn sift_down(&mut self, mut i: usize, end: usize) {
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut m = i;
            if l < end && self.before(self.at(m), self.at(l)) {
                m = l;
            }
            if r < end && self.before(self.at(m), self.at(r)) {
                m = r;
            }
            if m == i {
                return;
            }
            core::ptr::swap_nonoverlapping(self.at(i), self.at(m), self.rec);
            i = m;
        }
    }

    unsafe fn heapify(&mut self) {
        for i in (0..self.len / 2).rev() {
            self.sift_down(i, self.len);
        }
        self.worst = 0;
    }

    /// An owner's record offered: kept (representatives rendered, count mark cleared) when there is room or it beats the worst.
    unsafe fn offer(&mut self, r: *const u8) -> Result<(), i64> {
        let (mut tmp, mut done) = ([0u64; MAX_REPS], 0u32);
        let full = self.len == self.n;
        let slot = if !full {
            self.len += 1;
            self.len - 1
        } else {
            if !self.better(r, false, self.at(self.worst), &mut tmp, &mut done)? {
                return Ok(());
            }
            self.worst
        };
        let kept = self.at(slot);
        core::ptr::copy_nonoverlapping(r, kept, self.rec);
        let l = &self.spec.layout;
        let (mut k, mut t) = (0usize, 0usize);
        while k < l.nkeys() {
            if l.key_kind(k) == KeyKind::Fp128 {
                let (rep, lane) = if t < MAX_REPS { self.spec.reps[t] } else { (REP_NONE, 0) };
                if rep != REP_NONE && self.spec.tie_fp >> t & 1 == 0 {
                    let off = l.slot_off(rep as usize) as usize;
                    *(kept.add(off) as *mut u64) = self.cand_text(r, false, t, off, lane, &mut tmp, &mut done)?;
                }
                k += 2;
                t += 1;
                continue;
            }
            k += 1;
        }
        if self.spec.order_slot != ORDER_NONE {
            *(kept.add(self.off_count) as *mut u64) &= !MOVING;
        }
        if full {
            self.sift_down(0, self.len);
        } else if self.len == self.n {
            self.heapify();
        }
        Ok(())
    }

    /// The run in order, best first: the heap's sort in place.
    unsafe fn sort(&mut self) {
        self.heapify();
        for end in (1..self.len).rev() {
            core::ptr::swap_nonoverlapping(self.at(0), self.at(end), self.rec);
            self.sift_down(0, end);
        }
    }

    /// The `tie_fp` keys' representatives of the run's records into the run's form: the merge compared their words.
    unsafe fn render_ties(&mut self) -> Result<(), i64> {
        for t in 0..MAX_REPS {
            let (rep, lane) = self.spec.reps[t];
            if rep == REP_NONE || self.spec.tie_fp >> t & 1 == 0 {
                continue;
            }
            let off = self.spec.layout.slot_off(rep as usize) as usize;
            for i in (self.spec.skip as usize).min(self.len)..self.len {
                let (mut tmp, mut done) = ([0u64; MAX_REPS], 0u32);
                let r = self.at(i);
                *(r.add(off) as *mut u64) = self.cand_text(r, false, t, off, lane, &mut tmp, &mut done)?;
            }
        }
        Ok(())
    }
}

crate::contract::host! {
    /// The face. `partial(ctx)` is the finished leader region (`TABLE_OWNED`); a fresh call (`resume` `RESUME_NONE`) merges the owners' records into a run of at most `spec.n` in the arena and writes the header; a resumed call
    /// finds the run through it. `out` receives the run; `HOST_CONTINUE`, or a status below zero (`UNSUPPORTED_SHAPE`:
    /// not an owned region; `UNSUPPORTED_SCRATCH`: no arena room or table space; `ERROR`: a render failed, or no header).
    ///
    /// # Safety
    /// `ctx` is bound with `partial` at a leader region the owner phase finished; `spec` and `out` are live.
    #[unwind]
    pub unsafe fn leader_run(ctx: *mut Ctx, spec: *const LeaderSpec, out: *mut LeaderRun) -> i64 {
        let spec = &*spec;
        let own = partial(ctx);
        if own.is_null() || table_kind(own) != TABLE_OWNED {
            return unsupported(UNSUPPORTED_SHAPE);
        }
        let o = Owned::view(own);
        let h0 = OwnerTop::view(o.owner(0));
        if (o.owner_bytes as usize).saturating_sub(h0.table_off as usize) < HEADER_BYTES {
            return unsupported(UNSUPPORTED_SCRATCH);
        }
        let hdr = o.owner(0).add(h0.table_off as usize) as *mut Header;
        if (*ctx).resume != RESUME_NONE {
            if (*hdr).magic != LEADER_MAGIC || (*hdr).n != spec.n as u64 {
                gravity_fn::stash_error(Box::new(gravity_fn::PgError::error("gravity: a leader run resumed over an overwritten header"))); // alloc: Cold
                return ERROR;
            }
            *out = LeaderRun { base: word((*hdr).base as *mut u8), len: (*hdr).len };
            return HOST_CONTINUE;
        }
        let (rec, n) = (spec.layout.size() as usize, spec.n as usize);
        let base = if n == 0 { core::ptr::null_mut() } else { arena_alloc(ctx, n * rec) };
        if n > 0 && base.is_null() {
            return unsupported(UNSUPPORTED_SCRATCH);
        }
        let off_count = if spec.order_slot == ORDER_NONE { 0 } else { spec.layout.slot_off(spec.order_slot as usize) as usize };
        let mut m = Merge { spec, site: site_ptr(ctx, spec.site as usize), base, rec, n, len: 0, worst: 0, off_count };
        if n > 0 {
            for j in 0..o.nowners as usize {
                let h = OwnerTop::view(o.owner(j));
                for i in 0..h.len as usize {
                    if let Err(e) = m.offer(h.records().add(i * rec)) {
                        return e;
                    }
                }
            }
            m.sort();
            if let Err(e) = m.render_ties() {
                return e;
            }
        }
        *hdr = Header { magic: LEADER_MAGIC, n: n as u64, base: base as u64, len: m.len as u64 };
        *out = LeaderRun { base: word(base), len: m.len as u64 };
        HOST_CONTINUE
    }
}
