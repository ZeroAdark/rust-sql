//! ROWMAP2 M1 (pgrc-plans/ROWMAP2.md, ROWMAP-DELTA.md): a part's `RowIdentMap` as per-granule ID encodings, the
//! manifest form [`crate::manifest::PART_ROWMAP_GRANULES`]. The map's granules are [`GRANULE`] ordinals each, whatever
//! the part's own grain; a reader decodes IDs by ordinal range.
//!
//! A part is one of two kinds, decided from its IDs alone:
//!
//! - **MONO**: its ordinals cut into *fragments* (at every map granule's start, at every descent, and at every ascent
//!   of more than [`JUMP`] IDs) whose `[min, max]` spans are pairwise disjoint: sealed segments, bulk parts that homed
//!   several ranges, concatenations and DV rewrites, renumbered outputs. Each granule takes the smallest of
//!   [`G_DENSE`] (consecutive IDs: no payload), [`G_STRIDED`] (equal runs one stride apart: a fixed-width segment's
//!   blocks), [`G_RUNS`] (runs of consecutive IDs, FOR-packed) and [`G_DELTA`] (PFOR-delta frames of at most 128 rows,
//!   cut at fragment ends). ID → ordinal: the fragment directory (sorted by `min`), then a search inside one granule.
//! - **PERM**: a sorted merge's permutation. Every granule is [`G_DELTA`]: frames of at most 512 or 128 rows; each
//!   frame takes the smaller of zigzag deltas with high-bit patches (mode 0) and `delta - 1` with whole-value
//!   replacements (mode 1). The ID → ordinal direction is elected per part ([`RevForm`]):
//!   - [`RevForm::Fences`] when a 128-row frame's `[min, max]` fences overlap at most two frames per ID on average:
//!     the fences (persisted), sorted by `min`, then one frame decoded;
//!   - [`RevForm::Labels`]: the part's ID set `S` (runs), a group label per ID rank, the groups' frames (512 rows, cut
//!     at group starts), their fences, an anchor every [`ANCHOR`] rows, and the IDs no fast path finds (exceptions,
//!     sorted); persisted;
//!   - [`RevForm::FrameIx`]: `S` and a frame number per ID rank (128-row frames); NOT persisted: the forward frames
//!     are the whole sidecar and [`GranMap::build_frameix`] rebuilds the reverse on the first ID → ordinal use
//!     (pgrc_am charges it to the map cache).
//!   The election takes FENCES when its overlap rule holds, else the smaller of LABELS and FRAMEIX by exact size
//!   (in memory, both directions). Correctness never depends on the group heuristic: the builder replays every
//!   row's fast path and lists every row it would miss as an exception.
//!
//! - **RUN** (SORTRUNS-E0, [`RevForm::Run`]): a sorted run's member ([`crate::sortrun`]). Its ID → ordinal direction
//!   is the run's reverse file ([`crate::runrev`]), which names a *virtual frame* of the member: the run builder's
//!   frames (512 rows cut at group starts for a LABELS run, 128 rows for a FRAMEIX run). The map keeps them: each
//!   virtual frame's forward frames (more than one where a granule ends inside it, none once a rewrite dropped all
//!   its rows), and for a LABELS run each virtual frame's fence minimum as built (the fast path's candidate search),
//!   the forward frames' fences and an anchor every [`ANCHOR`] rows. A member rewritten in place (its DV folded)
//!   keeps its virtual frames, so the run's reverse stays valid. [`GranMap::run_find`] and [`GranMap::run_find_in`]
//!   answer the run's lookups; a lookup of the member alone ([`GranMap::ordinal_of`]) goes through a built FRAMEIX
//!   reverse ([`GranMap::needs_rev`]).
//!
//! Body (little-endian): a 40-byte header, the granule table (24 B a granule), the frame table (16 B a frame), the
//! fragment table (MONO, 24 B a fragment), the bit area (`u32` words count, then `u64` words), then the persisted
//! reverse (FENCES, LABELS) or the virtual frames (RUN). [`GranMap::decode`] validates everything a lookup reads and decodes every ID once (each
//! below `ROWID_LIMIT`, each fragment ascending inside its fences, each frame inside its fences), so no lookup on a
//! decoded map can index out of bounds.

use crate::rowid::{IDS_PER_BLOCK, ROWID_LIMIT};
use std::mem::size_of;

use crate::wire::{put_u32, put_u64, Cur};
use crate::{FormatError, FormatResult};

/// Ordinals per map granule.
pub const GRANULE: u32 = 8192;
/// The body's version byte.
pub const VERSION: u8 = 1;
/// A MONO fragment ends where the next ID climbs more than this past the last (16 heap blocks of IDs).
pub const JUMP: u64 = 16 * IDS_PER_BLOCK;
/// LABELS: an anchor every this many rows of a frame.
pub const ANCHOR: usize = 64;
/// Group heuristic: a group starts where an ID falls more than this below the last regular row.
const GROUP_T: u64 = 16 * IDS_PER_BLOCK;
/// Group heuristic: a row above the next this-many rows, while they continue the group, is a spike.
const GROUP_W: usize = 8;
/// `S`: a rank sample every this many runs.
const S_SAMPLE: usize = 16;
/// Frames: the large (LABELS) and the small (FRAMEIX, FENCES, MONO delta) frame, as log2 rows.
const LOG_LARGE: u8 = 9;
const LOG_SMALL: u8 = 7;
/// Frame table: the 24-bit start ordinal bounds a PERM or delta part's rows.
const MAX_FRAMED_ROWS: u64 = 1 << 24;

pub const G_DENSE: u8 = 0;
pub const G_RUNS: u8 = 1;
pub const G_STRIDED: u8 = 2;
pub const G_DELTA: u8 = 3;

const KIND_MONO: u8 = 0;
const KIND_PERM: u8 = 1;
const HEADER_LEN: usize = 40;
const GRAN_LEN: usize = 24;
const FRAME_LEN: usize = 16;
const FRAG_LEN: usize = 24;

/// A part's ID → ordinal form: [`RevForm::Mono`] for a MONO part, else the elected PERM reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevForm {
    Mono,
    Fences,
    Labels,
    FrameIx,
    /// A sorted run's member (SORTRUNS-E0): the run's reverse finds its IDs.
    Run,
}

impl RevForm {
    fn code(self) -> u8 {
        match self {
            RevForm::Mono => 0,
            RevForm::Fences => 1,
            RevForm::Labels => 2,
            RevForm::FrameIx => 3,
            RevForm::Run => 4,
        }
    }

    fn of(c: u8) -> Option<RevForm> {
        Some(match c {
            0 => RevForm::Mono,
            1 => RevForm::Fences,
            2 => RevForm::Labels,
            3 => RevForm::FrameIx,
            4 => RevForm::Run,
            _ => return None,
        })
    }

    /// The form's name in `pgrc_rowid_stats` and the `ROWMAP2_FORCE_REV` knob.
    pub fn name(self) -> &'static str {
        match self {
            RevForm::Mono => "mono",
            RevForm::Fences => "fences",
            RevForm::Labels => "labels",
            RevForm::FrameIx => "frameix",
            RevForm::Run => "run",
        }
    }

    pub fn from_name(s: &str) -> Option<RevForm> {
        [RevForm::Mono, RevForm::Fences, RevForm::Labels, RevForm::FrameIx].into_iter().find(|f| f.name() == s)
    }
}

/// What the encoder may be told: `force` makes a PERM part take that reverse (a test knob; every form answers every
/// ID, only the speed differs). A MONO part stays MONO.
#[derive(Debug, Clone, Copy, Default)]
pub struct Opts {
    pub force: Option<RevForm>,
}

// ---------------------------------------------------------------------------------------------------------------- bits

#[inline]
fn bits_for(x: u64) -> u32 {
    64 - x.leading_zeros()
}

#[inline]
fn mask(w: u32) -> u64 {
    if w >= 64 {
        u64::MAX
    } else {
        (1u64 << w) - 1
    }
}

#[inline]
fn zz(d: i64) -> u64 {
    ((d << 1) ^ (d >> 63)) as u64
}

#[inline]
fn unzz(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

/// A bit area being written: LSB first in `u64` words.
#[derive(Default, Debug, Clone)]
struct BitW {
    w: Vec<u64>,
    len: u64,
}

impl BitW {
    fn push(&mut self, v: u64, width: u32) {
        if width == 0 {
            return;
        }
        debug_assert!(width == 64 || v >> width == 0, "value {v} width {width}");
        let (i, o) = ((self.len >> 6) as usize, (self.len & 63) as u32);
        if i == self.w.len() {
            self.w.push(0);
        }
        self.w[i] |= v << o;
        if o + width > 64 {
            self.w.push(v >> (64 - o));
        }
        self.len += u64::from(width);
    }

    /// `vals` at `width` bits each, in order: as many [`BitW::push`]es, through one word held in a register.
    fn extend(&mut self, vals: impl IntoIterator<Item = u64>, width: u32) {
        if width == 0 {
            return;
        }
        let mut o = (self.len & 63) as u32;
        let mut acc = if o == 0 { 0 } else { self.w.pop().expect("the partial last word") };
        let mut k = 0u64;
        for v in vals {
            debug_assert!(width == 64 || v >> width == 0, "value {v} width {width}");
            acc |= v << o;
            let used = o + width;
            if used >= 64 {
                self.w.push(acc);
                acc = if used == 64 { 0 } else { v >> (64 - o) };
                o = used - 64;
            } else {
                o = used;
            }
            k += 1;
        }
        if o > 0 {
            self.w.push(acc);
        }
        self.len += k * u64::from(width);
    }

    fn put(&self, out: &mut Vec<u8>) {
        put_u32(out, self.w.len() as u32);
        for &x in &self.w {
            put_u64(out, x);
        }
    }
}

/// `width` bits at bit `pos` of `w` (the caller keeps `pos + width` inside the area; the area carries a zero word of
/// padding past its last, so a read never needs a word past it).
#[inline]
fn get(w: &[u64], pos: u64, width: u32) -> u64 {
    if width == 0 {
        return 0;
    }
    let (i, o) = ((pos >> 6) as usize, (pos & 63) as u32);
    let mut v = w[i] >> o;
    if o + width > 64 {
        v |= w[i + 1] << (64 - o);
    }
    v & mask(width)
}

/// A bit area read from a body: its words and one zero word of padding.
fn get_area(c: &mut Cur<'_>, at: &'static str) -> FormatResult<Box<[u64]>> {
    let n = c.u32(at)? as usize;
    if n.saturating_mul(8) > c.remaining() {
        return Err(FormatError::Truncated { at });
    }
    let mut w = crate::try_vec(n + 1, at)?;
    for _ in 0..n {
        w.push(c.u64(at)?);
    }
    w.push(0);
    Ok(w.into())
}

/// Whether `[pos, pos + bits)` lies inside an area of `words` words (padding excluded).
fn inside(words: usize, pos: u64, bits: u64) -> bool {
    pos.checked_add(bits).is_some_and(|end| end <= (words.saturating_sub(1) as u64) * 64)
}

// ------------------------------------------------------------------------------------------------------------- headers

/// A granule's header. `base`: DENSE the first ID, RUNS the runs' lowest start, STRIDED run 0's start. `count`: RUNS
/// its runs, STRIDED its runs after run 0, DELTA its frames. `off`: RUNS and STRIDED a bit offset (RUNS: `count`
/// starts at `w1` bits, then the start ordinals of runs 1.. at `w2`; STRIDED: `d1, stride, len, last len` at 32 bits
/// each), DELTA its first frame's index. `aux`: STRIDED run 0's length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Gran {
    pub form: u8,
    pub w1: u8,
    pub w2: u8,
    pub count: u32,
    pub base: u64,
    pub off: u32,
    pub aux: u32,
}

impl Gran {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&[self.form, self.w1, self.w2, 0]);
        put_u32(out, self.count);
        put_u64(out, self.base);
        put_u32(out, self.off);
        put_u32(out, self.aux);
    }

    fn get(c: &mut Cur<'_>) -> FormatResult<Gran> {
        const AT: &str = "RowIdentMap granule";
        let (form, w1, w2, _) = (c.u8(AT)?, c.u8(AT)?, c.u8(AT)?, c.u8(AT)?);
        Ok(Gran { form, w1, w2, count: c.u32(AT)?, base: c.u64(AT)?, off: c.u32(AT)?, aux: c.u32(AT)? })
    }
}

/// A delta frame's header: `a` = first ID (40 bits) | start ordinal << 40 (24 bits); `b` = bit offset (40 bits) |
/// width << 40 (7) | exception width << 47 (7) | mode << 54 (1) | exceptions << 55 (9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Frame {
    a: u64,
    b: u64,
}

impl Frame {
    #[allow(clippy::too_many_arguments)]
    fn new(first: u64, start: u32, off: u64, w: u32, ew: u32, mode: u8, nexc: u32) -> Frame {
        debug_assert!(first < ROWID_LIMIT && u64::from(start) < MAX_FRAMED_ROWS && off < 1 << 40 && w <= 64 && ew <= 64 && nexc < 512);
        Frame { a: first | u64::from(start) << 40, b: off | u64::from(w) << 40 | u64::from(ew) << 47 | u64::from(mode) << 54 | u64::from(nexc) << 55 }
    }
    #[inline]
    pub fn first(&self) -> u64 {
        self.a & mask(40)
    }
    #[inline]
    pub fn start(&self) -> u32 {
        (self.a >> 40) as u32
    }
    #[inline]
    fn off(&self) -> u64 {
        self.b & mask(40)
    }
    #[inline]
    fn w(&self) -> u32 {
        (self.b >> 40 & 0x7f) as u32
    }
    #[inline]
    fn ew(&self) -> u32 {
        (self.b >> 47 & 0x7f) as u32
    }
    #[inline]
    fn mode(&self) -> u8 {
        (self.b >> 54 & 1) as u8
    }
    #[inline]
    fn nexc(&self) -> u32 {
        (self.b >> 55) as u32
    }
}

/// A MONO fragment: ordinals from `ord` to the next fragment's (in ordinal order), ascending IDs in `[min, max]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Frag {
    pub min: u64,
    pub max: u64,
    pub ord: u32,
}

// ------------------------------------------------------------------------------------------------------------- encoder

/// The part's map body for row IDs `ids` (ordinal order, distinct, each below `ROWID_LIMIT`). `Corrupt` for a part too
/// large for the frame table (more than 2^24 rows; parts hold at most 2^20) when it needs frames.
pub fn encode(ids: &[u64], opts: Opts) -> FormatResult<Vec<u8>> {
    let frags = fragments(ids);
    if mono(&frags, ids.len() as u64) {
        return encode_mono(ids, &frags);
    }
    if ids.len() as u64 >= MAX_FRAMED_ROWS {
        return Err(FormatError::Corrupt { at: "RowIdentMap: a permuted part past 2^24 rows" });
    }
    encode_perm(ids, opts.force)
}

/// The map body of a part whose rows take `runs` = `(first ID, length)` in ordinal order (a seal's heap blocks, a bulk
/// part's ranges, a renumbered output): no per-row ID list unless the runs are not MONO.
pub fn encode_runs(runs: &[(u64, u32)], opts: Opts) -> FormatResult<Vec<u8>> {
    let mut frags: Vec<Frag> = Vec::new();
    let mut n = 0u64;
    let mut last: Option<u64> = None;
    for &(lo, len) in runs.iter().filter(|r| r.1 > 0) {
        let mut at = 0u32;
        while at < len {
            let ord = n + u64::from(at);
            let id = lo + u64::from(at);
            let room = (u64::from(GRANULE) - ord % u64::from(GRANULE)).min(u64::from(len - at)) as u32;
            let cut = ord % u64::from(GRANULE) == 0 || last.is_none_or(|l| id <= l || id - l > JUMP);
            match frags.last_mut() {
                Some(f) if !cut => f.max = id + u64::from(room) - 1,
                _ => frags.push(Frag { min: id, max: id + u64::from(room) - 1, ord: ord as u32 }),
            }
            last = Some(id + u64::from(room) - 1);
            at += room;
        }
        n += u64::from(len);
    }
    if u32::try_from(n).is_err() {
        return Err(FormatError::Corrupt { at: "RowIdentMap: a part past u32 rows" });
    }
    if !mono(&frags, n) {
        let ids: Vec<u64> = runs.iter().flat_map(|&(lo, len)| lo..lo + u64::from(len)).collect();
        return encode(&ids, opts);
    }
    // granule by granule: a cursor over the runs (run index, rows of it already taken)
    let mut buf: Vec<u64> = Vec::with_capacity(GRANULE as usize);
    let (mut ri, mut taken) = (0usize, 0u32);
    let ngran = n.div_ceil(u64::from(GRANULE)) as usize;
    let mut m = MonoW::new(n as u32, &frags);
    for g in 0..ngran {
        buf.clear();
        let want = (n - g as u64 * u64::from(GRANULE)).min(u64::from(GRANULE)) as usize;
        while buf.len() < want {
            let (lo, len) = runs[ri];
            let k = (len - taken).min((want - buf.len()) as u32);
            buf.extend((taken..taken + k).map(|j| lo + u64::from(j)));
            taken += k;
            if taken == len {
                (ri, taken) = (ri + 1, 0);
            }
        }
        m.granule(g, &buf)?;
    }
    m.finish()
}

/// MONO fragments of `ids` (cut at granule starts, descents and jumps past [`JUMP`]).
fn fragments(ids: &[u64]) -> Vec<Frag> {
    let mut out: Vec<Frag> = Vec::new();
    for (p, &id) in ids.iter().enumerate() {
        let cut = p % GRANULE as usize == 0 || id <= ids[p - 1] || id - ids[p - 1] > JUMP;
        match out.last_mut() {
            Some(f) if !cut => f.max = id,
            _ => out.push(Frag { min: id, max: id, ord: p as u32 }),
        }
    }
    out
}

/// A part of `rows` rows is MONO when its fragments are disjoint and few enough that their directory stays small (at
/// most one per 64 rows, or 16 past the granules): a part in descending ID order is a permutation, not a million
/// one-row fragments.
fn mono(frags: &[Frag], rows: u64) -> bool {
    frags.len() as u64 <= (rows / 64).max(rows.div_ceil(u64::from(GRANULE)) + 16) && disjoint(frags)
}

/// Whether fragments' spans are pairwise disjoint.
fn disjoint(frags: &[Frag]) -> bool {
    let mut s: Vec<(u64, u64)> = frags.iter().map(|f| (f.min, f.max)).collect();
    if !s.is_sorted() {
        s.sort_unstable();
    }
    s.windows(2).all(|w| w[0].1 < w[1].0)
}

/// Consecutive-ID runs of a granule's IDs: `(start, length)`.
fn runs_in(ids: &[u64]) -> Vec<(u64, u32)> {
    let mut out: Vec<(u64, u32)> = Vec::new();
    for &id in ids {
        match out.last_mut() {
            Some((s, l)) if *s + u64::from(*l) == id => *l += 1,
            _ => out.push((id, 1)),
        }
    }
    out
}

/// STRIDED parameters of runs, when they are: run 0 any; runs 1.. one `stride` apart from run 1's start, each `len`
/// long but the last (at most `len`); `(d1, stride, len, last)` with `d1` run 1's start past run 0's.
fn strided(runs: &[(u64, u32)]) -> Option<(u32, u32, u32, u32)> {
    if runs.len() < 2 {
        return None;
    }
    let rest = &runs[1..];
    let d1 = u32::try_from(rest[0].0.checked_sub(runs[0].0)?).ok()?;
    let len = rest[0].1;
    let stride = match rest.get(1) {
        Some(r) => u32::try_from(r.0.checked_sub(rest[0].0)?).ok()?,
        None => len,
    };
    let last = rest[rest.len() - 1].1;
    let ok = d1 >= runs[0].1
        && stride >= len
        && last <= len
        && rest.iter().enumerate().all(|(k, r)| r.0 == rest[0].0 + k as u64 * u64::from(stride) && (r.1 == len || k + 1 == rest.len()));
    ok.then_some((d1, stride, len, last))
}

/// A delta frame's layout: the smaller of mode 0 (zigzag deltas, a width `w` and the high bits of wider ones
/// patched in) and mode 1 (`delta - 1` at `w`, deltas below 1 or wider replaced whole, zigzag); `bits` is its payload
/// exactly (the frame's `w` bits per delta and its exceptions' index and `ew` bits).
struct Plan {
    w: u32,
    ew: u32,
    mode: u8,
    bits: u64,
}

/// The zigzag deltas of `ids` into `zs` (cleared first; one buffer serves a part's frames).
fn deltas(ids: &[u64], zs: &mut Vec<u64>) {
    zs.clear();
    zs.extend(ids.windows(2).map(|w| zz(w[1].wrapping_sub(w[0]) as i64)));
}

/// The layout of a frame whose deltas are `zs`, exception index width `ixw`.
fn plan(zs: &[u64], ixw: u32) -> Plan {
    let mut hist = [0u32; 65];
    let (mut hist1, mut mzz) = ([0u64; 65], [0u32; 65]);
    let (mut inv, mut inv_mzz) = (0u64, 0u32);
    for &z in zs {
        let bz = bits_for(z);
        hist[bz as usize] += 1;
        let d = unzz(z);
        if d >= 1 {
            let b = bits_for((d - 1) as u64) as usize;
            hist1[b] += 1;
            mzz[b] = mzz[b].max(bz);
        } else {
            inv += 1;
            inv_mzz = inv_mzz.max(bz);
        }
    }
    // mode 0
    let maxb = (0..65).rev().find(|&b| hist[b] > 0).unwrap_or(0) as u32;
    let (mut w0, mut c0, mut above) = (maxb, u64::MAX, 0u64);
    for w in (0..=maxb).rev() {
        let c = zs.len() as u64 * u64::from(w) + above * u64::from(ixw + (maxb - w));
        if c < c0 {
            (c0, w0) = (c, w);
        }
        above += u64::from(hist[w as usize]);
    }
    // mode 1
    let maxb1 = (0..65).rev().find(|&b| hist1[b] > 0).unwrap_or(0) as u32;
    let (mut c1, mut w1, mut ew1) = (u64::MAX, 0u32, 0u32);
    let (mut above1, mut amz) = (0u64, 0u32);
    for w in (0..=maxb1).rev() {
        let ne = inv + above1;
        let ew = inv_mzz.max(amz);
        let c = zs.len() as u64 * u64::from(w) + ne * u64::from(ixw + ew);
        if c < c1 {
            (c1, w1, ew1) = (c, w, ew);
        }
        above1 += hist1[w as usize];
        amz = amz.max(mzz[w as usize]);
    }
    if c1 < c0 {
        Plan { w: w1, ew: ew1, mode: 1, bits: c1 }
    } else {
        Plan { w: w0, ew: maxb - w0, mode: 0, bits: c0 }
    }
}

/// One delta frame of `ids` (its rows) appended to `bits` as [`plan`] lays it out (`zs`: a scratch buffer).
/// Returns `(w, ew, mode, nexc)`.
fn put_frame(bits: &mut BitW, ids: &[u64], ixw: u32, zs: &mut Vec<u64>) -> (u32, u32, u8, u32) {
    debug_assert!(!ids.is_empty());
    deltas(ids, zs);
    let zs = &zs[..];
    let p = plan(zs, ixw);
    let mut ne = 0;
    if p.mode == 1 {
        let fits = |z: u64| {
            let d = unzz(z);
            d >= 1 && bits_for((d - 1) as u64) <= p.w
        };
        bits.extend(zs.iter().map(|&z| if fits(z) { (unzz(z) - 1) as u64 } else { 0 }), p.w);
        for (j, &z) in zs.iter().enumerate() {
            if !fits(z) {
                bits.push(j as u64 + 1, ixw);
                bits.push(z, p.ew);
                ne += 1;
            }
        }
    } else {
        bits.extend(zs.iter().map(|&z| z & mask(p.w)), p.w);
        for (j, &z) in zs.iter().enumerate() {
            if bits_for(z) > p.w {
                bits.push(j as u64 + 1, ixw);
                bits.push(z >> p.w, p.ew);
                ne += 1;
            }
        }
    }
    debug_assert_eq!(u64::from(ne) * u64::from(ixw + p.ew) + zs.len() as u64 * u64::from(p.w), p.bits);
    (p.w, p.ew, p.mode, ne)
}

/// The bits [`frames_of`] writes for frames of `ids` every `f` rows with no other cuts (payload and frame table),
/// without writing them.
fn frames_bits(ids: &[u64], f: usize, log: u8) -> FormatResult<u64> {
    let mut zs = crate::try_vec(f, AT)?;
    Ok(ids
        .chunks(f)
        .map(|c| {
            deltas(c, &mut zs);
            plan(&zs, u32::from(log)).bits + FRAME_LEN as u64 * 8
        })
        .sum())
}

/// Frames of `ids[a..b]` (global ordinals) cut every `f` rows and at `cuts` (ascending global ordinals, extra cut
/// points), appended to `frames` and `bits`.
fn put_frames(frames: &mut Vec<Frame>, bits: &mut BitW, ids: &[u64], a: usize, b: usize, f: usize, cuts: &[usize], ixw: u32) -> FormatResult<()> {
    let mut zs = crate::try_vec(f, AT)?;
    let mut p = a;
    let mut c = cuts.partition_point(|&x| x <= a);
    while p < b {
        let mut e = (p + f).min(b);
        while c < cuts.len() && cuts[c] <= p {
            c += 1;
        }
        if c < cuts.len() && cuts[c] < e {
            e = cuts[c];
        }
        let off = bits.len;
        let (w, ew, mode, ne) = put_frame(bits, &ids[p..e], ixw, &mut zs);
        frames.push(Frame::new(ids[p], p as u32, off, w, ew, mode, ne));
        p = e;
    }
    Ok(())
}

/// The MONO writer: granule by granule.
struct MonoW<'a> {
    rows: u32,
    frags: &'a [Frag],
    grans: Vec<Gran>,
    frames: Vec<Frame>,
    bits: BitW,
}

impl<'a> MonoW<'a> {
    fn new(rows: u32, frags: &'a [Frag]) -> MonoW<'a> {
        MonoW { rows, frags, grans: Vec::new(), frames: Vec::new(), bits: BitW::default() }
    }

    /// Granule `g`'s IDs `ids`: the smallest form (ties to the faster: dense, strided, runs, delta).
    fn granule(&mut self, g: usize, ids: &[u64]) -> FormatResult<()> {
        let gb = g * GRANULE as usize;
        let n = ids.len();
        if n == 0 {
            return Ok(());
        }
        if ids[n - 1].wrapping_sub(ids[0]) == n as u64 - 1 && ids.windows(2).all(|w| w[1] == w[0] + 1) {
            self.grans.push(Gran { form: G_DENSE, base: ids[0], ..Gran::default() });
            return Ok(());
        }
        let runs = runs_in(ids);
        if let Some((d1, stride, len, last)) = strided(&runs) {
            let off = self.bits.len as u32;
            for v in [d1, stride, len, last] {
                self.bits.push(u64::from(v), 32);
            }
            self.grans.push(Gran { form: G_STRIDED, count: (runs.len() - 1) as u32, base: runs[0].0, off, aux: runs[0].1, ..Gran::default() });
            return Ok(());
        }
        let base = runs.iter().map(|r| r.0).min().expect("a run");
        let w1 = bits_for(runs.iter().map(|r| r.0 - base).max().unwrap_or(0));
        let w2 = bits_for(n as u64 - 1);
        let runs_bits = runs.len() as u64 * u64::from(w1) + (runs.len() as u64 - 1) * u64::from(w2);
        // delta frames cut at this granule's fragment starts
        let lo = self.frags.partition_point(|f| (f.ord as usize) < gb);
        let hi = self.frags.partition_point(|f| (f.ord as usize) < gb + n);
        let cuts: Vec<usize> = self.frags[lo..hi].iter().map(|f| f.ord as usize - gb).collect();
        let mut dbits = BitW::default();
        let mut dframes: Vec<Frame> = Vec::new();
        put_frames(&mut dframes, &mut dbits, ids, 0, n, 1 << LOG_SMALL, &cuts, u32::from(LOG_SMALL))?;
        let delta_bits = dbits.len + dframes.len() as u64 * (FRAME_LEN as u64 * 8);
        if runs_bits <= delta_bits || u64::from(self.rows) >= MAX_FRAMED_ROWS {
            let off = self.bits.len as u32;
            for r in &runs {
                self.bits.push(r.0 - base, w1);
            }
            let mut ord = 0u64;
            for (k, r) in runs.iter().enumerate() {
                if k > 0 {
                    self.bits.push(ord, w2);
                }
                ord += u64::from(r.1);
            }
            self.grans.push(Gran { form: G_RUNS, w1: w1 as u8, w2: w2 as u8, count: runs.len() as u32, base, off, aux: 0 });
            return Ok(());
        }
        let first = self.frames.len() as u32;
        let shift = self.bits.len;
        for f in &dframes {
            self.frames.push(Frame::new(f.first(), f.start() + gb as u32, f.off() + shift, f.w(), f.ew(), f.mode(), f.nexc()));
        }
        append_bits(&mut self.bits, &dbits);
        self.grans.push(Gran { form: G_DELTA, count: dframes.len() as u32, off: first, ..Gran::default() });
        Ok(())
    }

    fn finish(self) -> FormatResult<Vec<u8>> {
        let mut out = header(KIND_MONO, RevForm::Mono, LOG_SMALL, self.rows, self.grans.len(), self.frags.len(), self.frames.len())?;
        body_tables(&mut out, &self.grans, &self.frames);
        for f in self.frags {
            put_u64(&mut out, f.min);
            put_u64(&mut out, f.max);
            put_u32(&mut out, f.ord);
            put_u32(&mut out, 0);
        }
        self.bits.put(&mut out);
        Ok(out)
    }
}

/// `src` appended to `dst` bit for bit.
fn append_bits(dst: &mut BitW, src: &BitW) {
    let mut left = src.len;
    let mut i = 0;
    while left > 0 {
        let w = left.min(64) as u32;
        dst.push(src.w[i] & mask(w), w);
        left -= u64::from(w);
        i += 1;
    }
}

fn header(kind: u8, rev: RevForm, log: u8, rows: u32, ngran: usize, n3: usize, nframes: usize) -> FormatResult<Vec<u8>> {
    let mut out = crate::try_vec(HEADER_LEN + ngran * GRAN_LEN + nframes * FRAME_LEN + 64, AT)?;
    out.extend_from_slice(&[VERSION, kind, rev.code(), log]);
    put_u32(&mut out, rows);
    put_u32(&mut out, ngran as u32);
    put_u32(&mut out, n3 as u32);
    put_u32(&mut out, nframes as u32);
    out.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(out.len(), HEADER_LEN);
    Ok(out)
}

fn body_tables(out: &mut Vec<u8>, grans: &[Gran], frames: &[Frame]) {
    for g in grans {
        g.put(out);
    }
    for f in frames {
        put_u64(out, f.a);
        put_u64(out, f.b);
    }
}

fn encode_mono(ids: &[u64], frags: &[Frag]) -> FormatResult<Vec<u8>> {
    let rows = u32::try_from(ids.len()).map_err(|_| FormatError::Corrupt { at: "RowIdentMap: a part past u32 rows" })?;
    let mut m = MonoW::new(rows, frags);
    for (g, chunk) in ids.chunks(GRANULE as usize).enumerate() {
        m.granule(g, chunk)?;
    }
    m.finish()
}

/// `keys` sorted (LSD radix, 11-bit digits over the low `bits` bits; a digit every key shares is skipped).
pub fn try_radix_sort(keys: &mut Vec<u64>, bits: u32) -> FormatResult<()> {
    const D: u32 = 11;
    let n = keys.len();
    if n < 256 {
        keys.sort_unstable();
        return Ok(());
    }
    let mut tmp = crate::try_filled(n, 0u64, AT)?;
    let mut shift = 0;
    while shift < bits {
        let mut count = vec![0usize; 1 << D];
        for &k in keys.iter() {
            count[(k >> shift & mask(D)) as usize] += 1;
        }
        if count.iter().any(|&c| c == n) {
            shift += D;
            continue;
        }
        let mut sum = 0;
        for c in count.iter_mut() {
            let t = *c;
            *c = sum;
            sum += t;
        }
        for &k in keys.iter() {
            let d = (k >> shift & mask(D)) as usize;
            tmp[count[d]] = k;
            count[d] += 1;
        }
        std::mem::swap(keys, &mut tmp);
        shift += D;
    }
    Ok(())
}

#[cfg(test)]
pub fn radix_sort(keys: &mut Vec<u64>, bits: u32) {
    try_radix_sort(keys, bits).expect("radix_sort");
}

/// Group starts (row positions) of a permuted part's IDs (ROWMAP-DELTA's heuristic): a group starts where an ID falls
/// more than [`GROUP_T`] below the last regular row; a row above the next [`GROUP_W`] rows while those continue the
/// group is a spike (it stays, not regular).
fn groups_of(ids: &[u64]) -> Vec<usize> {
    let n = ids.len();
    let mut gs = vec![0usize];
    if n == 0 {
        return gs;
    }
    let mut prev = ids[0];
    for p in 1..n {
        let x = ids[p];
        if p + 1 < n {
            let m = match ids.get(p + 1..p + 1 + GROUP_W) {
                Some(w) => <&[u64; GROUP_W]>::try_from(w).expect("GROUP_W rows").iter().fold(u64::MAX, |a, &b| a.min(b)),
                None => *ids[p + 1..].iter().min().expect("a row"),
            };
            if x > m.saturating_add(GROUP_T) && m.saturating_add(GROUP_T) >= prev {
                continue;
            }
        }
        if x.saturating_add(GROUP_T) < prev {
            gs.push(p);
        }
        prev = x;
    }
    gs
}

/// `S` as written: runs of consecutive IDs of the sorted ID set.
struct SetW {
    nruns: u32,
    gw: u32,
    lw: u32,
    first: u64,
    bits: BitW,
}

/// (A repeated ID is dropped: the set then holds fewer IDs than the part has rows, which [`IdSet::of`] refuses.)
fn set_of(sorted: impl Iterator<Item = u64>) -> SetW {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for x in sorted {
        match runs.last_mut() {
            Some((lo, len)) if *lo + *len == x => *len += 1,
            Some((lo, len)) if x < *lo + *len => {}
            _ => runs.push((x, 1)),
        }
    }
    set_of_runs(&runs)
}

/// The set of the IDs `lo + i` whose bit `i` of `bm` is set (as [`set_of`] over them in order), a word at a time.
fn set_of_bitmap(lo: u64, bm: &[u64]) -> SetW {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for (i, &w0) in bm.iter().enumerate() {
        let (mut w, mut at) = (w0, lo + i as u64 * 64);
        while w != 0 {
            let tz = w.trailing_zeros();
            w >>= tz;
            at += u64::from(tz);
            let ones = w.trailing_ones();
            match runs.last_mut() {
                Some((s, len)) if *s + *len == at => *len += u64::from(ones),
                _ => runs.push((at, u64::from(ones))),
            }
            at += u64::from(ones);
            w = w.checked_shr(ones).unwrap_or(0);
        }
    }
    set_of_runs(&runs)
}

/// `S` from its runs `(first ID, length)`, ascending and apart.
fn set_of_runs(runs: &[(u64, u64)]) -> SetW {
    let gaps = runs.windows(2).map(|w| w[1].0 - w[0].0 - w[0].1);
    let gw = bits_for(gaps.clone().max().unwrap_or(0));
    let lw = bits_for(runs.iter().map(|r| r.1 - 1).max().unwrap_or(0));
    let mut bits = BitW::default();
    bits.extend(gaps, gw);
    bits.extend(runs.iter().map(|r| r.1 - 1), lw);
    SetW { nruns: runs.len() as u32, gw, lw, first: runs.first().map_or(0, |r| r.0), bits }
}

impl SetW {
    fn put(&self, out: &mut Vec<u8>) {
        put_u32(out, self.nruns);
        out.extend_from_slice(&[self.gw as u8, self.lw as u8, 0, 0]);
        put_u64(out, self.first);
        self.bits.put(out);
    }

    /// Bits in memory: the runs and their samples.
    fn mem_bits(&self) -> u64 {
        self.bits.len + 64 + (u64::from(self.nruns) / S_SAMPLE as u64 + 1) * 96
    }
}

/// A PERM part's forward frames as written.
struct FramesW {
    frames: Vec<Frame>,
    bits: BitW,
    fmin: Vec<u64>,
    fmax: Vec<u64>,
}

impl FramesW {
    fn bits(&self) -> u64 {
        self.bits.len + self.frames.len() as u64 * FRAME_LEN as u64 * 8
    }
}

fn frames_of(ids: &[u64], f: usize, cuts: &[usize], log: u8) -> FormatResult<FramesW> {
    let mut frames = Vec::new();
    let mut bits = BitW::default();
    put_frames(&mut frames, &mut bits, ids, 0, ids.len(), f, cuts, u32::from(log))?;
    let mut fmin = crate::try_vec(frames.len(), AT)?;
    let mut fmax = crate::try_vec(frames.len(), AT)?;
    for k in 0..frames.len() {
        let (s, e) = (frames[k].start() as usize, frames.get(k + 1).map_or(ids.len(), |f| f.start() as usize));
        let (lo, hi) = ids[s..e].iter().fold((u64::MAX, 0u64), |(l, h), &x| (l.min(x), h.max(x)));
        fmin.push(lo);
        fmax.push(hi);
    }
    Ok(FramesW { frames, bits, fmin, fmax })
}

/// The granule table of a PERM part: every granule delta, its frames from the frame table.
fn perm_grans(rows: usize, frames: &[Frame]) -> FormatResult<Vec<Gran>> {
    let ngran = rows.div_ceil(GRANULE as usize);
    let mut out = crate::try_vec(ngran, AT)?;
    let mut k = 0usize;
    for g in 0..ngran {
        let first = k;
        while k < frames.len() && (frames[k].start() as usize) < (g + 1) * GRANULE as usize {
            k += 1;
        }
        out.push(Gran { form: G_DELTA, count: (k - first) as u32, off: first as u32, ..Gran::default() });
    }
    Ok(out)
}

fn put_fences(out: &mut Vec<u8>, fr: &FramesW) {
    for k in 0..fr.frames.len() {
        put_u64(out, fr.fmin[k]);
        put_u64(out, fr.fmax[k]);
    }
}

/// Each row's rank in ID order, and the ID set. IDs spanning under 64 per row (a sorted merge of sealed segments)
/// rank through a bitmap of the span and per-word prefix counts, in one pass; others (or repeated IDs) through the
/// LSD radix sort of (ID, row).
fn ranks_and_set(ids: &[u64]) -> FormatResult<(Vec<u32>, SetW)> {
    let n = ids.len();
    let (lo, hi) = ids.iter().fold((u64::MAX, 0u64), |(l, h), &x| (l.min(x), h.max(x)));
    if n > 0 && hi - lo < 64 * n as u64 {
        let words = ((hi - lo) / 64 + 1) as usize;
        let mut bm = crate::try_filled(words, 0u64, AT)?;
        for &x in ids {
            let d = x - lo;
            bm[(d >> 6) as usize] |= 1u64 << (d & 63);
        }
        let mut pre = crate::try_filled(words, 0u32, AT)?;
        let mut c = 0u32;
        for (p, &w) in pre.iter_mut().zip(&bm) {
            *p = c;
            c += w.count_ones();
        }
        if c as usize == n {
            let rank = ids
                .iter()
                .map(|&x| {
                    let d = x - lo;
                    let w = (d >> 6) as usize;
                    pre[w] + (bm[w] & ((1u64 << (d & 63)) - 1)).count_ones()
                })
                .collect();
            return Ok((rank, set_of_bitmap(lo, &bm)));
        }
    }
    let pb = bits_for(n as u64);
    let mut keys: Vec<u64> = ids.iter().enumerate().map(|(p, &id)| id << pb | p as u64).collect();
    try_radix_sort(&mut keys, 40 + pb)?;
    let mut rank = crate::try_filled(n, 0u32, AT)?;
    for (r, &k) in keys.iter().enumerate() {
        rank[(k & mask(pb)) as usize] = r as u32;
    }
    Ok((rank, set_of(keys.iter().map(|&k| k >> pb))))
}

fn encode_perm(ids: &[u64], force: Option<RevForm>) -> FormatResult<Vec<u8>> {
    let n = ids.len();
    let (rank, s) = ranks_and_set(ids)?;
    let granule_cuts: Vec<usize> = (1..n.div_ceil(GRANULE as usize)).map(|g| g * GRANULE as usize).collect();
    // the small frames are every 2^LOG_SMALL rows (a granule end is one of their ends): written only for the form
    // that takes them, FENCES or FRAMEIX
    const { assert!(GRANULE % (1 << LOG_SMALL) == 0) };
    let fs = 1usize << LOG_SMALL;
    // FENCES: average frames whose fence holds an ID (ranks are monotone in ID: a fence's ID span is its rank span)
    let over: u64 = rank
        .chunks(fs)
        .map(|c| {
            let (lo, hi) = c.iter().fold((u32::MAX, 0u32), |(l, h), &r| (l.min(r), h.max(r)));
            u64::from(hi - lo + 1)
        })
        .sum();
    let fences = force == Some(RevForm::Fences) || (force.is_none() && over <= 2 * n as u64);
    if fences {
        let small = frames_of(ids, fs, &granule_cuts, LOG_SMALL)?;
        let mut out = header(KIND_PERM, RevForm::Fences, LOG_SMALL, n as u32, n.div_ceil(GRANULE as usize), 0, small.frames.len())?;
        body_tables(&mut out, &perm_grans(n, &small.frames)?, &small.frames);
        small.bits.put(&mut out);
        put_fences(&mut out, &small);
        return Ok(out);
    }
    // LABELS: frames cut at granule ends and group starts
    let gs = groups_of(ids);
    let mut cuts: Vec<usize> = gs[1..].iter().copied().chain(granule_cuts.iter().copied()).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let large = frames_of(ids, 1 << LOG_LARGE, &cuts, LOG_LARGE)?;
    let ng = gs.len();
    let mut gfirst: Vec<u32> = crate::try_vec(ng + 1, AT)?;
    {
        let mut k = 0usize;
        for &g0 in &gs {
            while k < large.frames.len() && (large.frames[k].start() as usize) < g0 {
                k += 1;
            }
            gfirst.push(k as u32);
        }
        gfirst.push(large.frames.len() as u32);
    }
    // replay: a row whose frame is not the fast path's candidate or a neighbour is an exception
    // the label `ng` marks an exception
    let lw = bits_for(ng as u64);
    let mut lab_by_rank = crate::try_filled(n, 0u32, AT)?;
    let mut exc: Vec<(u64, u32)> = Vec::new();
    for g in 0..ng {
        let (fa, fb) = (gfirst[g] as usize, gfirst[g + 1] as usize);
        for kk in fa..fb {
            let (a, b) = (large.frames[kk].start() as usize, large.frames.get(kk + 1).map_or(n, |f| f.start() as usize));
            for p in a..b {
                let k = cand(&large.fmin[fa..fb], ids[p]) + fa;
                if kk.abs_diff(k) <= 1 {
                    lab_by_rank[rank[p] as usize] = g as u32;
                } else {
                    lab_by_rank[rank[p] as usize] = ng as u32;
                    exc.push((ids[p], p as u32));
                }
            }
        }
    }
    exc.sort_unstable();
    let mut anchors: Vec<u64> = Vec::new();
    for kk in 0..large.frames.len() {
        let (a, b) = (large.frames[kk].start() as usize, large.frames.get(kk + 1).map_or(n, |f| f.start() as usize));
        anchors.extend((a..b).step_by(ANCHOR).map(|p| ids[p] - large.fmin[kk]));
    }
    let aw = bits_for(anchors.iter().copied().max().unwrap_or(0));
    let labels_bits = large.bits() + s.mem_bits() + n as u64 * u64::from(lw) + (ng as u64 + 1) * 32 + large.frames.len() as u64 * 128 + anchors.len() as u64 * u64::from(aw) + exc.len() as u64 * 96;
    let labels = match force {
        Some(RevForm::Labels) => true,
        Some(RevForm::FrameIx) => false,
        _ => {
            let fiw = bits_for(n.div_ceil(fs) as u64 - 1);
            let small_bits = frames_bits(ids, fs, LOG_SMALL)?;
            debug_assert_eq!(small_bits, frames_of(ids, fs, &granule_cuts, LOG_SMALL)?.bits());
            labels_bits < small_bits + s.mem_bits() + n as u64 * u64::from(fiw)
        }
    };
    if !labels {
        let small = frames_of(ids, fs, &granule_cuts, LOG_SMALL)?;
        let mut out = header(KIND_PERM, RevForm::FrameIx, LOG_SMALL, n as u32, n.div_ceil(GRANULE as usize), 0, small.frames.len())?;
        body_tables(&mut out, &perm_grans(n, &small.frames)?, &small.frames);
        small.bits.put(&mut out);
        return Ok(out);
    }
    let mut out = header(KIND_PERM, RevForm::Labels, LOG_LARGE, n as u32, n.div_ceil(GRANULE as usize), ng, large.frames.len())?;
    body_tables(&mut out, &perm_grans(n, &large.frames)?, &large.frames);
    large.bits.put(&mut out);
    s.put(&mut out);
    let mut lb = BitW::default();
    lb.extend(lab_by_rank.iter().map(|&l| u64::from(l)), lw);
    out.extend_from_slice(&[lw as u8, 0, 0, 0]);
    lb.put(&mut out);
    for &g in &gfirst {
        put_u32(&mut out, g);
    }
    put_fences(&mut out, &large);
    let mut ab = BitW::default();
    ab.extend(anchors.iter().copied(), aw);
    out.extend_from_slice(&[aw as u8, 0, 0, 0]);
    ab.put(&mut out);
    put_u32(&mut out, exc.len() as u32);
    for &(id, p) in &exc {
        put_u64(&mut out, id);
        put_u32(&mut out, p);
    }
    Ok(out)
}

/// The fast path's candidate frame (an index into `fmin`) for `id` among one group's frames.
#[inline]
fn cand(fmin: &[u64], id: u64) -> usize {
    fmin.partition_point(|&m| m <= id).saturating_sub(1)
}

/// RUN: the fast path's candidate virtual frame (an index into `vfmin`, a LABELS group's frames' fence minimums) for
/// `id`; [`GranMap::run_find`] tries it and its neighbours. The run builder replays it to list the IDs it misses.
#[inline]
pub fn run_cand(vfmin: &[u64], id: u64) -> usize {
    cand(vfmin, id)
}

/// The large and small frame's rows (a LABELS and a FRAMEIX run's virtual frames).
pub const RUN_FRAME_LARGE: u32 = 1 << LOG_LARGE;
pub const RUN_FRAME_SMALL: u32 = 1 << LOG_SMALL;

/// RUN (SORTRUNS-E0): a sorted run's member's map. `ids` are its rows' IDs in ordinal order (distinct, below
/// `ROWID_LIMIT`, at least one, under 2^24); `vstarts` the virtual frames' first rows (from 0, ascending; a start
/// repeated, or equal to the rows, is a frame a rewrite emptied), each at most [`RUN_FRAME_LARGE`] rows for a LABELS
/// run's member (`vmin` = each virtual frame's fence minimum as the run builder set it: at or below its IDs) or
/// [`RUN_FRAME_SMALL`] for a FRAMEIX run's (`vmin` = `None`). `EncodeContract` for anything else.
pub fn encode_run_member(ids: &[u64], vstarts: &[u32], vmin: Option<&[u64]>) -> FormatResult<Vec<u8>> {
    let n = ids.len();
    let log = if vmin.is_some() { LOG_LARGE } else { LOG_SMALL };
    let f = 1usize << log;
    let bad = |detail| Err(FormatError::EncodeContract { detail });
    if n == 0 || n as u64 >= MAX_FRAMED_ROWS || ids.iter().any(|&x| x >= ROWID_LIMIT) {
        return bad("run member map: rows");
    }
    let nv = vstarts.len();
    let vend = |v: usize| vstarts.get(v + 1).map_or(n, |&e| e as usize);
    if nv == 0 || vstarts[0] != 0 || vstarts.windows(2).any(|w| w[0] > w[1]) || vstarts[nv - 1] as usize > n || (0..nv).any(|v| vend(v) - vstarts[v] as usize > f) {
        return bad("run member map: virtual frames");
    }
    if let Some(vm) = vmin {
        if vm.len() != nv || (0..nv).any(|v| vm[v] >= ROWID_LIMIT || ids[vstarts[v] as usize..vend(v)].iter().any(|&x| x < vm[v])) {
            return bad("run member map: fence minimums");
        }
    }
    let mut cuts: Vec<usize> = (1..n.div_ceil(GRANULE as usize)).map(|g| g * GRANULE as usize).collect();
    cuts.extend(vstarts[1..].iter().map(|&s| s as usize).filter(|&s| s < n));
    cuts.sort_unstable();
    cuts.dedup();
    let fr = frames_of(ids, f, &cuts, log)?;
    let mut vfirst: Vec<u32> = vstarts.iter().map(|&s| fr.frames.partition_point(|x| x.start() < s) as u32).collect();
    vfirst.push(fr.frames.len() as u32);
    let mut out = header(KIND_PERM, RevForm::Run, log, n as u32, n.div_ceil(GRANULE as usize), nv, fr.frames.len())?;
    body_tables(&mut out, &perm_grans(n, &fr.frames)?, &fr.frames);
    fr.bits.put(&mut out);
    for &v in &vfirst {
        put_u32(&mut out, v);
    }
    let Some(vm) = vmin else { return Ok(out) };
    for &x in vm {
        put_u64(&mut out, x);
    }
    put_fences(&mut out, &fr);
    let mut anchors: Vec<u64> = Vec::new();
    for kk in 0..fr.frames.len() {
        let (a, b) = (fr.frames[kk].start() as usize, fr.frames.get(kk + 1).map_or(n, |x| x.start() as usize));
        anchors.extend((a..b).step_by(ANCHOR).map(|p| ids[p] - fr.fmin[kk]));
    }
    let aw = bits_for(anchors.iter().copied().max().unwrap_or(0));
    let mut ab = BitW::default();
    ab.extend(anchors.iter().copied(), aw);
    out.extend_from_slice(&[aw as u8, 0, 0, 0]);
    ab.put(&mut out);
    Ok(out)
}

// ------------------------------------------------------------------------------------------------------------- decoder

/// A decoded map. The forward direction is always here; the reverse of a FRAMEIX part is built on demand
/// ([`GranMap::build_frameix`]) and handed back to [`GranMap::ordinal_of`].
#[derive(Debug)]
pub struct GranMap {
    rows: u32,
    rev_form: RevForm,
    log: u8,
    grans: Box<[Gran]>,
    frames: Box<[Frame]>,
    frags: Box<[Frag]>,
    /// MONO: the fragments in ID order, and their mins (one contiguous search per lookup).
    by_min: Box<[u32]>,
    smin: Box<[u64]>,
    bits: Box<[u64]>,
    rev: Option<Reverse>,
    disk: usize,
}

/// A PERM part's ID → ordinal structure.
#[derive(Debug)]
pub enum Reverse {
    Fences(Fences),
    Labels(Labels),
    FrameIx(FrameIx),
    /// A run member's virtual frames (not an ID → ordinal structure of its own: [`GranMap::run_find`]).
    Run(RunFrames),
}

/// The part's sorted ID set as runs of consecutive IDs, with a rank sample every [`S_SAMPLE`] runs.
#[derive(Debug)]
pub struct IdSet {
    nruns: usize,
    gw: u32,
    lw: u32,
    loff: u64,
    bits: Box<[u64]>,
    slo: Box<[u64]>,
    srank: Box<[u32]>,
}

#[derive(Debug)]
pub struct Fences {
    fmin: Box<[u64]>,
    fmax: Box<[u64]>,
    by_min: Box<[u32]>,
    smin: Box<[u64]>,
    maxspan: u64,
}

#[derive(Debug)]
pub struct Labels {
    s: IdSet,
    lw: u32,
    labels: Box<[u64]>,
    xsym: u64,
    gfirst: Box<[u32]>,
    fmin: Box<[u64]>,
    fmax: Box<[u64]>,
    aw: u32,
    anchors: Box<[u64]>,
    aoff: Box<[u32]>,
    exc_id: Box<[u64]>,
    exc_ord: Box<[u32]>,
}

#[derive(Debug)]
pub struct FrameIx {
    s: IdSet,
    w: u32,
    fi: Box<[u64]>,
}

/// A run member's virtual frames: virtual frame `v` is forward frames `vfirst[v]..vfirst[v + 1]`. A LABELS run's
/// member (large frames) also keeps each virtual frame's fence minimum as built, and the forward frames' fences and
/// anchors (empty for a FRAMEIX run's member).
#[derive(Debug)]
pub struct RunFrames {
    vfirst: Box<[u32]>,
    vfmin: Box<[u64]>,
    fmin: Box<[u64]>,
    fmax: Box<[u64]>,
    aw: u32,
    anchors: Box<[u64]>,
    aoff: Box<[u32]>,
}

const AT: &str = "RowIdentMap granules";

fn corrupt() -> FormatError {
    FormatError::Corrupt { at: AT }
}

fn check(ok: bool) -> FormatResult<()> {
    if ok {
        Ok(())
    } else {
        Err(corrupt())
    }
}

impl IdSet {
    fn from_parts(nruns: usize, gw: u32, lw: u32, first: u64, bits: Box<[u64]>, rows: u64) -> FormatResult<IdSet> {
        check(gw <= 40 && lw <= 40 && (nruns > 0 || rows == 0))?;
        let loff = (nruns.saturating_sub(1) as u64) * u64::from(gw);
        check(inside(bits.len(), 0, loff + nruns as u64 * u64::from(lw)))?;
        let (mut slo, mut srank) = (crate::try_vec(nruns / S_SAMPLE + 1, AT)?, crate::try_vec(nruns / S_SAMPLE + 1, AT)?);
        let (mut lo, mut r) = (first, 0u64);
        for k in 0..nruns {
            if k > 0 {
                let len_prev = get(&bits, loff + (k as u64 - 1) * u64::from(lw), lw) + 1;
                lo = lo.checked_add(len_prev + get(&bits, (k as u64 - 1) * u64::from(gw), gw)).ok_or_else(corrupt)?;
            }
            let len = get(&bits, loff + k as u64 * u64::from(lw), lw) + 1;
            check(lo.checked_add(len).is_some_and(|e| e <= ROWID_LIMIT))?;
            if k % S_SAMPLE == 0 {
                slo.push(lo);
                srank.push(r as u32);
            }
            r += len;
        }
        check(r == rows)?;
        Ok(IdSet { nruns, gw, lw, loff, bits, slo: slo.into(), srank: srank.into() })
    }

    fn of(w: SetW, rows: u64) -> FormatResult<IdSet> {
        let mut bits = w.bits.w;
        bits.push(0);
        IdSet::from_parts(w.nruns as usize, w.gw, w.lw, w.first, bits.into(), rows)
    }

    fn get(c: &mut Cur<'_>, rows: u64) -> FormatResult<IdSet> {
        let nruns = c.u32(AT)? as usize;
        let (gw, lw, _, _) = (c.u8(AT)?, c.u8(AT)?, c.u8(AT)?, c.u8(AT)?);
        let first = c.u64(AT)?;
        let bits = get_area(c, AT)?;
        IdSet::from_parts(nruns, u32::from(gw), u32::from(lw), first, bits, rows)
    }

    #[inline]
    fn len(&self, k: usize) -> u64 {
        get(&self.bits, self.loff + k as u64 * u64::from(self.lw), self.lw) + 1
    }

    #[inline]
    fn gap(&self, k: usize) -> u64 {
        get(&self.bits, (k as u64 - 1) * u64::from(self.gw), self.gw)
    }

    /// `id`'s rank in the set, `None` when the set lacks it.
    pub fn rank(&self, id: u64) -> Option<u32> {
        let s = self.slo.partition_point(|&x| x <= id).checked_sub(1)?;
        let mut k = s * S_SAMPLE;
        let end = (k + S_SAMPLE).min(self.nruns);
        let (mut lo, mut r) = (self.slo[s], u64::from(self.srank[s]));
        loop {
            let len = self.len(k);
            if id < lo + len {
                return (id >= lo).then(|| (r + id - lo) as u32);
            }
            r += len;
            k += 1;
            if k >= end {
                return None;
            }
            lo += len + self.gap(k);
        }
    }

    fn mem_bytes(&self) -> usize {
        self.bits.len() * 8 + self.slo.len() * 8 + self.srank.len() * 4
    }
}

impl GranMap {
    /// Decode and validate a map body for a part of `rows` rows (see the module docs for what is checked).
    pub fn decode(body: &[u8], rows: u64) -> FormatResult<GranMap> {
        let mut c = Cur::new(body);
        let (version, kind, rev, log) = (c.u8(AT)?, c.u8(AT)?, c.u8(AT)?, c.u8(AT)?);
        let (n, ngran, n3, nframes) = (c.u32(AT)?, c.u32(AT)? as usize, c.u32(AT)? as usize, c.u32(AT)? as usize);
        c.take(20, AT)?;
        if version != VERSION {
            return Err(FormatError::BadVersion { at: AT, got: u32::from(version) });
        }
        let rev_form = RevForm::of(rev).ok_or_else(corrupt)?;
        check(u64::from(n) == rows && ngran as u64 == rows.div_ceil(u64::from(GRANULE)))?;
        check(match rev_form {
            RevForm::Mono => kind == KIND_MONO && log == LOG_SMALL,
            RevForm::Labels => kind == KIND_PERM && log == LOG_LARGE,
            RevForm::Run => kind == KIND_PERM && (log == LOG_LARGE || log == LOG_SMALL) && rows >= 1,
            _ => kind == KIND_PERM && log == LOG_SMALL,
        })?;
        check(nframes == 0 || rows < MAX_FRAMED_ROWS)?;
        check(ngran.saturating_mul(GRAN_LEN) <= c.remaining() && nframes.saturating_mul(FRAME_LEN) <= c.remaining())?;
        let mut grans = crate::try_vec(ngran, AT)?;
        for _ in 0..ngran {
            grans.push(Gran::get(&mut c)?);
        }
        let mut frames = crate::try_vec(nframes, AT)?;
        for _ in 0..nframes {
            frames.push(Frame { a: c.u64(AT)?, b: c.u64(AT)? });
        }
        let mut frags = Vec::new();
        if rev_form == RevForm::Mono {
            check(n3.saturating_mul(FRAG_LEN) <= c.remaining())?;
            for _ in 0..n3 {
                let (min, max, ord) = (c.u64(AT)?, c.u64(AT)?, c.u32(AT)?);
                c.u32(AT)?;
                frags.push(Frag { min, max, ord });
            }
        }
        let bits = get_area(&mut c, AT)?;
        let mut m = GranMap { rows: n, rev_form, log, grans: grans.into(), frames: frames.into(), frags: frags.into(), by_min: Box::new([]), smin: Box::new([]), bits, rev: None, disk: body.len() };
        m.check_layout()?;
        m.rev = match rev_form {
            RevForm::Mono | RevForm::FrameIx => None,
            RevForm::Fences => Some(Reverse::Fences(Fences::new(get_fences(&mut c, nframes)?))),
            RevForm::Labels => Some(Reverse::Labels(m.get_labels(&mut c, n3)?)),
            RevForm::Run => Some(Reverse::Run(m.get_run(&mut c, n3)?)),
        };
        check(c.remaining() == 0)?;
        if rev_form == RevForm::Mono {
            let mut by: Vec<u32> = (0..m.frags.len() as u32).collect();
            by.sort_unstable_by_key(|&i| m.frags[i as usize].min);
            check(by.windows(2).all(|w| m.frags[w[0] as usize].max < m.frags[w[1] as usize].min))?;
            m.smin = by.iter().map(|&i| m.frags[i as usize].min).collect();
            m.by_min = by.into();
        }
        m.check_ids()?;
        Ok(m)
    }

    /// Rows of map granule `g`.
    #[inline]
    fn rows_in(&self, g: usize) -> u32 {
        (self.rows - g as u32 * GRANULE).min(GRANULE)
    }

    /// Frame `k`'s rows. Frames never cross a granule end (every kind cuts them there), and a MONO map's next frame
    /// may start granules later (only DELTA granules hold frames), so the frame ends at the next frame's start or its
    /// granule's end, whichever is first.
    #[inline]
    fn frame_rows(&self, k: usize) -> u32 {
        let s = self.frames[k].start();
        let gend = (s / GRANULE + 1).saturating_mul(GRANULE).min(self.rows);
        self.frames.get(k + 1).map_or(gend, |f| f.start().min(gend)) - s
    }

    fn strided_params(&self, gr: &Gran) -> (u64, u64, u64, u64) {
        let p = u64::from(gr.off);
        (get(&self.bits, p, 32), get(&self.bits, p + 32, 32), get(&self.bits, p + 64, 32), get(&self.bits, p + 96, 32))
    }

    #[inline]
    fn run_start(&self, gr: &Gran, k: u32) -> u64 {
        get(&self.bits, u64::from(gr.off) + u64::from(k) * u64::from(gr.w1), u32::from(gr.w1))
    }

    #[inline]
    fn run_ord(&self, gr: &Gran, k: u32) -> u32 {
        if k == 0 {
            return 0;
        }
        let at = u64::from(gr.off) + u64::from(gr.count) * u64::from(gr.w1) + u64::from(k - 1) * u64::from(gr.w2);
        get(&self.bits, at, u32::from(gr.w2)) as u32
    }

    /// The run of a RUNS granule holding local ordinal `l`.
    fn run_of(&self, gr: &Gran, l: u32) -> u32 {
        let (mut lo, mut hi) = (0u32, gr.count);
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if self.run_ord(gr, mid) <= l {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// The frame of granule `gr` (DELTA) holding global ordinal `o`.
    fn frame_of(&self, gr: &Gran, o: u32) -> usize {
        let fs = &self.frames[gr.off as usize..(gr.off + gr.count) as usize];
        gr.off as usize + fs.partition_point(|f| f.start() <= o) - 1
    }

    /// Layout checks: every granule's fields, every frame's bit range and widths, the fragments' ordinals.
    fn check_layout(&self) -> FormatResult<()> {
        let words = self.bits.len();
        let mut next_frame = 0usize;
        for (g, gr) in self.grans.iter().enumerate() {
            let rg = u64::from(self.rows_in(g));
            let gb = g as u64 * u64::from(GRANULE);
            check(self.rev_form == RevForm::Mono || gr.form == G_DELTA)?;
            match gr.form {
                G_DENSE => check(gr.base.checked_add(rg).is_some_and(|e| e <= ROWID_LIMIT))?,
                G_STRIDED => {
                    check(inside(words, u64::from(gr.off), 128))?;
                    let (d1, stride, len, last) = self.strided_params(gr);
                    let (len0, r) = (u64::from(gr.aux), u64::from(gr.count));
                    check(len0 >= 1 && r >= 1 && r <= rg && len >= 1 && len <= rg && (1..=len).contains(&last) && stride >= len && d1 >= len0)?;
                    check(len0 + (r - 1) * len + last == rg)?;
                    let max = gr.base.checked_add(d1 + (r - 1) * stride + last - 1);
                    check(max.is_some_and(|m| m < ROWID_LIMIT))?;
                }
                G_RUNS => {
                    let (r, w1, w2) = (u64::from(gr.count), u32::from(gr.w1), u32::from(gr.w2));
                    check(r >= 1 && r <= rg && w1 <= 40 && w2 <= 16)?;
                    check(inside(words, u64::from(gr.off), r * u64::from(w1) + (r - 1) * u64::from(w2)))?;
                    let mut prev = 0u32;
                    for k in 0..gr.count {
                        let o = self.run_ord(gr, k);
                        check(k == 0 || (o > prev && u64::from(o) < rg))?;
                        prev = o;
                        let len = if k + 1 < gr.count { u64::from(self.run_ord(gr, k + 1)) } else { rg }.saturating_sub(u64::from(o));
                        let end = gr.base.checked_add(self.run_start(gr, k)).and_then(|s| s.checked_add(len));
                        check(end.is_some_and(|e| e <= ROWID_LIMIT))?;
                    }
                }
                G_DELTA => {
                    check(gr.off as usize == next_frame && gr.count >= 1)?;
                    let end = next_frame + gr.count as usize;
                    check(end <= self.frames.len())?;
                    for k in next_frame..end {
                        let f = &self.frames[k];
                        let s = u64::from(f.start());
                        let e = if k + 1 < end { u64::from(self.frames[k + 1].start()) } else { gb + rg };
                        check((k > next_frame || s == gb) && s < e && e <= gb + rg && e - s <= 1 << self.log)?;
                        let (w, ew, ne) = (f.w(), f.ew(), u64::from(f.nexc()));
                        check(w <= 64 && ew <= 64 && ne < e - s)?;
                        check(f.mode() == 1 || ne == 0 || (ew >= 1 && w + ew <= 64))?;
                        check(inside(words, f.off(), (e - s - 1) * u64::from(w) + ne * u64::from(u32::from(self.log) + ew)))?;
                    }
                    next_frame = end;
                }
                _ => return Err(corrupt()),
            }
        }
        check(next_frame == self.frames.len())?;
        if self.rev_form == RevForm::Mono {
            check(self.frags.is_empty() == (self.rows == 0) && self.frags.first().is_none_or(|f| f.ord == 0))?;
            for (i, f) in self.frags.iter().enumerate() {
                let end = self.frags.get(i + 1).map_or(self.rows, |x| x.ord);
                check(f.ord < end && f.ord / GRANULE == (end - 1) / GRANULE && f.min <= f.max)?;
                let gr = &self.grans[(f.ord / GRANULE) as usize];
                let l = f.ord % GRANULE;
                match gr.form {
                    // frames and runs are cut at fragment starts
                    G_DELTA => check(self.frames[self.frame_of(gr, f.ord)].start() == f.ord)?,
                    G_RUNS => check(self.run_ord(gr, self.run_of(gr, l)) == l)?,
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Every ID once: below `ROWID_LIMIT`; MONO fragments ascending, from `min` to `max`; PERM frames inside their
    /// fences (when the reverse keeps fences); every frame's exception indices ascending inside the frame.
    fn check_ids(&self) -> FormatResult<()> {
        let mut buf = vec![0u64; GRANULE as usize];
        for k in 0..self.frames.len() {
            let f = &self.frames[k];
            let m = self.frame_rows(k) as u64;
            let ixw = u32::from(self.log);
            let es = u64::from(ixw + f.ew());
            let eb = f.off() + (m - 1) * u64::from(f.w());
            let mut prev = 0u64;
            for e in 0..u64::from(f.nexc()) {
                let j = get(&self.bits, eb + e * es, ixw);
                check(j > prev && j < m)?;
                prev = j;
            }
        }
        let fences = match &self.rev {
            Some(Reverse::Fences(x)) => Some((&x.fmin, &x.fmax)),
            Some(Reverse::Labels(x)) => Some((&x.fmin, &x.fmax)),
            Some(Reverse::Run(x)) if !x.fmin.is_empty() => Some((&x.fmin, &x.fmax)),
            _ => None,
        };
        for g in 0..self.grans.len() {
            let rg = self.rows_in(g) as usize;
            let gb = g as u32 * GRANULE;
            self.decode_ids(gb, &mut buf[..rg]);
            check(buf[..rg].iter().all(|&x| x < ROWID_LIMIT))?;
            if let Some((lo, hi)) = fences {
                let gr = &self.grans[g];
                for k in gr.off as usize..(gr.off + gr.count) as usize {
                    let s = (self.frames[k].start() - gb) as usize;
                    let e = s + self.frame_rows(k) as usize;
                    check(buf[s..e].iter().all(|&x| lo[k] <= x && x <= hi[k]))?;
                }
            }
        }
        if self.rev_form == RevForm::Mono {
            for (i, f) in self.frags.iter().enumerate() {
                let end = self.frags.get(i + 1).map_or(self.rows, |x| x.ord);
                let len = (end - f.ord) as usize;
                self.decode_ids(f.ord, &mut buf[..len]);
                check(buf[0] == f.min && buf[len - 1] == f.max && buf[..len].windows(2).all(|w| w[0] < w[1]))?;
            }
        }
        Ok(())
    }

    fn get_labels(&self, c: &mut Cur<'_>, ng: usize) -> FormatResult<Labels> {
        let rows = u64::from(self.rows);
        let s = IdSet::get(c, rows)?;
        let lw = u32::from(c.u8(AT)?);
        c.take(3, AT)?;
        let labels = get_area(c, AT)?;
        let xsym = ng as u64;
        check(ng >= 1 && lw == bits_for(xsym) && inside(labels.len(), 0, rows * u64::from(lw)))?;
        check((0..rows).all(|r| get(&labels, r * u64::from(lw), lw) <= xsym))?;
        check((ng + 1).saturating_mul(4) <= c.remaining())?;
        let mut gfirst = crate::try_vec(ng + 1, AT)?;
        for _ in 0..=ng {
            gfirst.push(c.u32(AT)?);
        }
        check(gfirst[0] == 0 && gfirst[ng] as usize == self.frames.len() && gfirst.windows(2).all(|w| w[0] < w[1]))?;
        let (fmin, fmax) = get_fences(c, self.frames.len())?;
        let aw = u32::from(c.u8(AT)?);
        c.take(3, AT)?;
        let anchors = get_area(c, AT)?;
        let mut aoff = crate::try_vec(self.frames.len() + 1, AT)?;
        let mut na = 0u32;
        for k in 0..self.frames.len() {
            aoff.push(na);
            na += self.frame_rows(k).div_ceil(ANCHOR as u32);
        }
        aoff.push(na);
        check(aw <= 40 && inside(anchors.len(), 0, u64::from(na) * u64::from(aw)))?;
        let ne = c.u32(AT)? as usize;
        check(ne.saturating_mul(12) <= c.remaining())?;
        let (mut exc_id, mut exc_ord) = (crate::try_vec(ne, AT)?, crate::try_vec(ne, AT)?);
        for _ in 0..ne {
            exc_id.push(c.u64(AT)?);
            exc_ord.push(c.u32(AT)?);
        }
        check(exc_id.windows(2).all(|w| w[0] < w[1]) && exc_ord.iter().all(|&o| o < self.rows))?;
        Ok(Labels { s, lw, labels, xsym, gfirst: gfirst.into(), fmin, fmax, aw, anchors, aoff: aoff.into(), exc_id: exc_id.into(), exc_ord: exc_ord.into() })
    }

    /// RUN: the virtual frames, their forward frames, and (large frames) their fence minimums, the forward frames'
    /// fences and anchors. Every virtual frame at most a frame of rows; each fence minimum at or below its forward
    /// frames' (they only lose rows).
    fn get_run(&self, c: &mut Cur<'_>, nv: usize) -> FormatResult<RunFrames> {
        let nframes = self.frames.len();
        check(nv >= 1 && (nv + 1).saturating_mul(4) <= c.remaining())?;
        let mut vfirst = crate::try_vec(nv + 1, AT)?;
        for _ in 0..=nv {
            vfirst.push(c.u32(AT)?);
        }
        check(vfirst[0] == 0 && vfirst[nv] as usize == nframes && vfirst.windows(2).all(|w| w[0] <= w[1]))?;
        let start = |k: usize| if k < nframes { self.frames[k].start() } else { self.rows };
        check(vfirst.windows(2).all(|w| start(w[1] as usize) - start(w[0] as usize) <= 1 << self.log))?;
        if self.log == LOG_SMALL {
            return Ok(RunFrames { vfirst: vfirst.into(), vfmin: Box::new([]), fmin: Box::new([]), fmax: Box::new([]), aw: 0, anchors: Box::new([]), aoff: Box::new([]) });
        }
        check(nv.saturating_mul(8) <= c.remaining())?;
        let mut vfmin = crate::try_vec(nv, AT)?;
        for _ in 0..nv {
            vfmin.push(c.u64(AT)?);
        }
        let (fmin, fmax) = get_fences(c, nframes)?;
        check(vfmin.iter().all(|&x| x < ROWID_LIMIT))?;
        check((0..nv).all(|v| (vfirst[v]..vfirst[v + 1]).all(|k| vfmin[v] <= fmin[k as usize])))?;
        let aw = u32::from(c.u8(AT)?);
        c.take(3, AT)?;
        let anchors = get_area(c, AT)?;
        let mut aoff = crate::try_vec(nframes + 1, AT)?;
        let mut na = 0u32;
        for k in 0..nframes {
            aoff.push(na);
            na += self.frame_rows(k).div_ceil(ANCHOR as u32);
        }
        aoff.push(na);
        check(aw <= 40 && inside(anchors.len(), 0, u64::from(na) * u64::from(aw)))?;
        Ok(RunFrames { vfirst: vfirst.into(), vfmin: vfmin.into(), fmin, fmax, aw, anchors, aoff: aoff.into() })
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// The part's elected reverse form.
    pub fn rev_form(&self) -> RevForm {
        self.rev_form
    }

    /// Whether an ID → ordinal lookup needs a reverse built by [`Self::build_frameix`] (a run member's own lookup
    /// too: the run's reverse is the fast one).
    pub fn needs_rev(&self) -> bool {
        matches!(self.rev_form, RevForm::FrameIx | RevForm::Run)
    }

    /// Granules per form ([`G_DENSE`], [`G_RUNS`], [`G_STRIDED`], [`G_DELTA`]).
    pub fn granule_forms(&self) -> [u32; 4] {
        let mut c = [0u32; 4];
        for g in self.grans.iter() {
            c[g.form as usize] += 1;
        }
        c
    }

    pub fn frames(&self) -> usize {
        self.frames.len()
    }

    /// The body's bytes on disk.
    pub fn disk_bytes(&self) -> usize {
        self.disk
    }

    /// The bytes the decoded map holds (the forward direction and a persisted reverse; a built FRAMEIX reverse
    /// reports its own, [`Reverse::mem_bytes`]).
    pub fn mem_bytes(&self) -> usize {
        self.grans.len() * size_of::<Gran>()
            + self.frames.len() * size_of::<Frame>()
            + self.frags.len() * size_of::<Frag>()
            + self.by_min.len() * 4
            + self.smin.len() * 8
            + self.bits.len() * 8
            + self.rev.as_ref().map_or(0, Reverse::mem_bytes)
    }

    /// Ordinal `o`'s ID (the caller bounds `o` by the rows).
    pub fn ident(&self, o: u32) -> u64 {
        let mut one = [0u64; 1];
        self.decode_ids(o, &mut one);
        one[0]
    }

    /// The IDs of ordinals `o..o + out.len()` (the caller bounds them by the rows).
    pub fn decode_ids(&self, o: u32, out: &mut [u64]) {
        let mut done = 0usize;
        while done < out.len() {
            let at = o + done as u32;
            let g = (at / GRANULE) as usize;
            let l = at % GRANULE;
            let take = ((self.rows_in(g) - l) as usize).min(out.len() - done);
            self.fill(g, l, &mut out[done..done + take]);
            done += take;
        }
    }

    /// Granule `g`'s IDs from local ordinal `l` into `out` (inside the granule).
    fn fill(&self, g: usize, l: u32, out: &mut [u64]) {
        let gr = &self.grans[g];
        match gr.form {
            G_DENSE => {
                let b = gr.base + u64::from(l);
                out.iter_mut().enumerate().for_each(|(i, x)| *x = b + i as u64);
            }
            G_STRIDED => {
                // the first run, then whole runs one stride apart: one division for the starting run, fill loops after
                let (d1, stride, len, _) = self.strided_params(gr);
                let len0 = u64::from(gr.aux);
                let li = u64::from(l);
                let lead = len0.saturating_sub(li).min(out.len() as u64) as usize;
                out[..lead].iter_mut().enumerate().for_each(|(j, x)| *x = gr.base + li + j as u64);
                let mut i = lead;
                if i < out.len() {
                    let e = li + lead as u64 - len0;
                    let (mut r, mut run) = (e % len, gr.base + d1 + e / len * stride);
                    while i < out.len() {
                        let take = ((len - r) as usize).min(out.len() - i);
                        let s = run + r;
                        out[i..i + take].iter_mut().enumerate().for_each(|(j, x)| *x = s + j as u64);
                        i += take;
                        r = 0;
                        run += stride;
                    }
                }
            }
            G_RUNS => {
                let mut k = self.run_of(gr, l);
                let mut li = l;
                let mut i = 0usize;
                while i < out.len() {
                    let (ord, next) = (self.run_ord(gr, k), if k + 1 < gr.count { self.run_ord(gr, k + 1) } else { u32::MAX });
                    let s = gr.base + self.run_start(gr, k) + u64::from(li - ord);
                    let take = ((next - li) as usize).min(out.len() - i);
                    out[i..i + take].iter_mut().enumerate().for_each(|(j, x)| *x = s + j as u64);
                    i += take;
                    li += take as u32;
                    k += 1;
                }
            }
            _ => {
                let gb = g as u32 * GRANULE;
                let o = gb + l;
                let mut k = self.frame_of(gr, o);
                let mut tmp = [0u64; 1 << LOG_LARGE];
                let mut i = 0usize;
                while i < out.len() {
                    let (s, m) = (self.frames[k].start(), self.frame_rows(k) as usize);
                    let from = (o + i as u32 - s) as usize;
                    let take = (m - from).min(out.len() - i);
                    if from == 0 && take == m {
                        self.decode_frame(k, &mut out[i..i + m]);
                    } else {
                        self.decode_frame(k, &mut tmp[..m]);
                        out[i..i + take].copy_from_slice(&tmp[from..from + take]);
                    }
                    i += take;
                    k += 1;
                }
            }
        }
    }

    /// Frame `k`'s IDs (`out.len()` = its rows).
    fn decode_frame(&self, k: usize, out: &mut [u64]) {
        let f = self.frames[k];
        let (w, ew, ixw) = (f.w(), f.ew(), u32::from(self.log));
        let bits = &self.bits;
        out[0] = f.first();
        let mut p = f.off();
        if w > 0 {
            for x in out[1..].iter_mut() {
                *x = get(bits, p, w);
                p += u64::from(w);
            }
        } else {
            out[1..].iter_mut().for_each(|x| *x = 0);
        }
        let es = u64::from(ixw + ew);
        if f.mode() == 1 {
            out[1..].iter_mut().for_each(|x| *x = x.wrapping_add(1));
            for e in 0..u64::from(f.nexc()) {
                let j = get(bits, p + e * es, ixw) as usize;
                out[j] = unzz(get(bits, p + e * es + u64::from(ixw), ew)) as u64;
            }
        } else {
            for e in 0..u64::from(f.nexc()) {
                let j = get(bits, p + e * es, ixw) as usize;
                out[j] |= get(bits, p + e * es + u64::from(ixw), ew) << w;
            }
            out[1..].iter_mut().for_each(|x| *x = unzz(*x) as u64);
        }
        let mut acc = out[0];
        for x in out[1..].iter_mut() {
            acc = acc.wrapping_add(*x);
            *x = acc;
        }
    }

    /// Rows `j0 + 1 .. jend` of frame `k`, row `j0`'s ID being `acc0`: the ordinal of `id` among rows `j0..jend` (one
    /// fused pass: unpack, patch, prefix sum, compare, stopping at the match).
    fn find_from(&self, k: usize, id: u64, j0: usize, acc0: u64, jend: usize) -> Option<u32> {
        let f = self.frames[k];
        let s = f.start();
        if acc0 == id {
            return Some(s + j0 as u32);
        }
        let m = self.frame_rows(k) as u64;
        let (w, ew, ixw) = (f.w(), f.ew(), u32::from(self.log));
        let bits = &self.bits;
        let ne = u64::from(f.nexc());
        let eb = f.off() + (m - 1) * u64::from(w);
        let es = u64::from(ixw + ew);
        let idx = |e: u64| if e < ne { get(bits, eb + e * es, ixw) as usize } else { usize::MAX };
        let mut e = 0u64;
        let mut next = idx(0);
        while next <= j0 {
            e += 1;
            next = idx(e);
        }
        let mut acc = acc0;
        let mut p = f.off() + j0 as u64 * u64::from(w);
        for j in j0 + 1..jend {
            let mut z = get(bits, p, w);
            p += u64::from(w);
            let delta = if f.mode() == 1 {
                if j == next {
                    let v = unzz(get(bits, eb + e * es + u64::from(ixw), ew)) as u64;
                    e += 1;
                    next = idx(e);
                    v
                } else {
                    z.wrapping_add(1)
                }
            } else {
                if j == next {
                    z |= get(bits, eb + e * es + u64::from(ixw), ew) << w;
                    e += 1;
                    next = idx(e);
                }
                unzz(z) as u64
            };
            acc = acc.wrapping_add(delta);
            if acc == id {
                return Some(s + j as u32);
            }
        }
        None
    }

    /// `id` in frame `k` (a full frame pass).
    #[inline]
    fn find_in(&self, k: usize, id: u64) -> Option<u32> {
        self.find_from(k, id, 0, self.frames[k].first(), self.frame_rows(k) as usize)
    }

    /// The ordinal holding `id`, `None` when the part has none. A FRAMEIX part answers through `built` (its reverse,
    /// [`Self::build_frameix`]); without it, `None`.
    pub fn ordinal_of(&self, id: u64, built: Option<&Reverse>) -> Option<u32> {
        match self.rev_form {
            RevForm::Mono => self.mono_ordinal_of(id),
            _ => match self.own_rev().or(built)? {
                Reverse::Fences(x) => x.lookup(self, id),
                Reverse::Labels(x) => x.lookup(self, id),
                Reverse::FrameIx(x) => x.lookup(self, id),
                Reverse::Run(_) => None,
            },
        }
    }

    /// The persisted reverse an ID → ordinal lookup uses (a run member's virtual frames are not one).
    fn own_rev(&self) -> Option<&Reverse> {
        self.rev.as_ref().filter(|r| !matches!(r, Reverse::Run(_)))
    }

    fn run_frames(&self) -> FormatResult<&RunFrames> {
        match &self.rev {
            Some(Reverse::Run(x)) => Ok(x),
            _ => Err(corrupt()),
        }
    }

    /// RUN: the member's virtual frames (`Corrupt` for another form).
    pub fn vframes(&self) -> FormatResult<u32> {
        Ok(self.run_frames()?.vfirst.len() as u32 - 1)
    }

    /// RUN (SORTRUNS-E0 L3b1): the member's virtual frames as [`encode_run_member`] takes them: each one's first row
    /// (a frame a rewrite emptied repeats the next one's), and (a LABELS run's member) each one's fence minimum
    /// (`None` for a FRAMEIX run's). A member rewritten in place keeps them, so the run's reverse stays valid.
    /// `Corrupt` for another form.
    pub fn run_vframes(&self) -> FormatResult<(Vec<u32>, Option<Vec<u64>>)> {
        let x = self.run_frames()?;
        let start = |k: usize| self.frames.get(k).map_or(self.rows, |f| f.start());
        let vstarts = x.vfirst[..x.vfirst.len() - 1].iter().map(|&k| start(k as usize)).collect();
        Ok((vstarts, (!x.vfmin.is_empty()).then(|| crate::try_to_vec(&x.vfmin[..], AT)).transpose()?))
    }

    /// RUN, a LABELS run's group: `id` among virtual frames `va..vb` of this member, the fast path's candidate (the
    /// last whose fence minimum is at or below `id`) and its neighbours ([`run_cand`]; the run builder lists every ID
    /// they miss as an exception). `Corrupt` for frames the map lacks or a member of a FRAMEIX run.
    pub fn run_find(&self, va: u32, vb: u32, id: u64) -> FormatResult<Option<u32>> {
        let x = self.run_frames()?;
        let nv = x.vfirst.len() - 1;
        check(va < vb && vb as usize <= nv && !x.vfmin.is_empty())?;
        let (a, b) = (va as usize, vb as usize);
        let k = a + run_cand(&x.vfmin[a..b], id);
        for v in [Some(k), k.checked_sub(1).filter(|&v| v >= a), Some(k + 1).filter(|&v| v < b)].into_iter().flatten() {
            if let Some(o) = self.find_in_vframe(x, v, id) {
                return Ok(Some(o));
            }
        }
        Ok(None)
    }

    /// RUN: `id` in virtual frame `v` of this member (an exception's frame, a FRAMEIX run's frame).
    pub fn run_find_in(&self, v: u32, id: u64) -> FormatResult<Option<u32>> {
        let x = self.run_frames()?;
        check((v as usize) < x.vfirst.len() - 1)?;
        Ok(self.find_in_vframe(x, v as usize, id))
    }

    fn find_in_vframe(&self, x: &RunFrames, v: usize, id: u64) -> Option<u32> {
        (x.vfirst[v] as usize..x.vfirst[v + 1] as usize).find_map(|k| {
            if x.fmin.is_empty() {
                return self.find_in(k, id);
            }
            if id < x.fmin[k] || id > x.fmax[k] {
                return None;
            }
            find_anchored(self, k, id, x.fmin[k], x.aw, &x.anchors, x.aoff[k], x.aoff[k + 1])
        })
    }

    fn mono_ordinal_of(&self, id: u64) -> Option<u32> {
        let i = self.smin.partition_point(|&x| x <= id).checked_sub(1)?;
        let fi = self.by_min[i] as usize;
        let fr = &self.frags[fi];
        if id > fr.max {
            return None;
        }
        let (oa, ob) = (fr.ord, self.frags.get(fi + 1).map_or(self.rows, |x| x.ord));
        let g = (oa / GRANULE) as usize;
        let gb = g as u32 * GRANULE;
        let gr = &self.grans[g];
        let o = match gr.form {
            G_DENSE => gb + u32::try_from(id - gr.base).ok()?,
            G_STRIDED => {
                let (d1, stride, len, last) = self.strided_params(gr);
                let d = id.checked_sub(gr.base)?;
                let len0 = u64::from(gr.aux);
                let local = if d < len0 {
                    d
                } else {
                    let e = d.checked_sub(d1)?;
                    let (k, r) = (e / stride, e % stride);
                    let lk = if k + 1 == u64::from(gr.count) { last } else { len };
                    if k >= u64::from(gr.count) || r >= lk {
                        return None;
                    }
                    len0 + k * len + r
                };
                gb + local as u32
            }
            G_RUNS => {
                let off = id.checked_sub(gr.base)?;
                // the fragment's runs; a fragment from the granule's start or to its end (one fragment per granule
                // is the sealed case) skips that end's search
                let ka = if oa == gb { 0 } else { self.run_of(gr, oa - gb) };
                let kb = if ob == gb + self.rows_in(g) { gr.count - 1 } else { self.run_of(gr, ob - 1 - gb) };
                // the last run in ka..=kb starting at or below `off` (the fragment's runs ascend)
                let (mut lo, mut hi) = (ka, kb + 1);
                while hi - lo > 1 {
                    let mid = lo + (hi - lo) / 2;
                    if self.run_start(gr, mid) <= off {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                let st = self.run_start(gr, lo);
                let ord = self.run_ord(gr, lo);
                let len = if lo + 1 < gr.count { self.run_ord(gr, lo + 1) } else { self.rows_in(g) } - ord;
                let d = off.checked_sub(st)?;
                if d >= u64::from(len) {
                    return None;
                }
                gb + ord + d as u32
            }
            _ => {
                let fs = &self.frames[gr.off as usize..(gr.off + gr.count) as usize];
                let (a, b) = (fs.partition_point(|f| f.start() < oa), fs.partition_point(|f| f.start() < ob));
                let k = gr.off as usize + a + fs[a..b].partition_point(|f| f.first() <= id).checked_sub(1)?;
                return self.find_in(k, id);
            }
        };
        (oa <= o && o < ob).then_some(o)
    }

    /// FRAMEIX: the reverse, built from the forward frames (every ID decoded once, sorted with its frame number).
    /// `Corrupt` when an ID repeats (the forward frames decoded, but they are not a part's map).
    pub fn build_frameix(&self) -> FormatResult<Reverse> {
        let n = self.rows as usize;
        let mut ids = crate::try_filled(n, 0u64, AT)?;
        self.decode_ids(0, &mut ids);
        let fb = bits_for(self.frames.len() as u64);
        let w = bits_for(self.frames.len().saturating_sub(1) as u64);
        let mut fi = BitW::default();
        let s = if 40 + fb <= 64 {
            for k in 0..self.frames.len() {
                let (s, m) = (self.frames[k].start() as usize, self.frame_rows(k) as usize);
                ids[s..s + m].iter_mut().for_each(|x| *x = *x << fb | k as u64);
            }
            try_radix_sort(&mut ids, 40 + fb)?;
            for &k in &ids {
                fi.push(k & mask(fb), w);
            }
            set_of(ids.iter().map(|&k| k >> fb))
        } else {
            let mut pairs: Vec<(u64, u32)> = crate::try_vec(n, AT)?;
            for k in 0..self.frames.len() {
                let (s, m) = (self.frames[k].start() as usize, self.frame_rows(k) as usize);
                pairs.extend(ids[s..s + m].iter().map(|&x| (x, k as u32)));
            }
            pairs.sort_unstable();
            for &(_, k) in &pairs {
                fi.push(u64::from(k), w);
            }
            set_of(pairs.iter().map(|p| p.0))
        };
        fi.w.push(0);
        let s = IdSet::of(s, n as u64)?;
        Ok(Reverse::FrameIx(FrameIx { s, w, fi: fi.w.into() }))
    }

    /// The ordinals of `ids` into `out` (`out.len() == ids.len()`), through `built` as [`Self::ordinal_of`]: a PERM
    /// part decodes each frame the IDs hit once when they are many (at least one ID per two frames), else one lookup
    /// each.
    pub fn ordinals_of(&self, ids: &[u64], built: Option<&Reverse>, out: &mut [Option<u32>]) {
        assert_eq!(ids.len(), out.len(), "one answer per ID");
        let rev = self.own_rev().or(built);
        let few = ids.len() * 2 < self.frames.len();
        let bucketed = !few && matches!(rev, Some(Reverse::Labels(_) | Reverse::FrameIx(_)));
        let mut want: Vec<(u32, u32)> = Vec::new();
        if !bucketed || want.try_reserve(ids.len()).is_err() {
            for (o, &id) in out.iter_mut().zip(ids) {
                *o = self.ordinal_of(id, built);
            }
            return;
        }
        for (i, &id) in ids.iter().enumerate() {
            out[i] = None;
            let k = match rev {
                Some(Reverse::FrameIx(x)) => x.frame(id),
                Some(Reverse::Labels(x)) => match x.cand_frame(id) {
                    Ok(k) => k,
                    Err(direct) => {
                        out[i] = direct;
                        None
                    }
                },
                _ => None,
            };
            if let Some(k) = k {
                want.push((k as u32, i as u32));
            }
        }
        want.sort_unstable_by_key(|&(k, i)| (k, ids[i as usize]));
        let mut tmp = [0u64; 1 << LOG_LARGE];
        let mut srt: Vec<(u64, u32)> = Vec::with_capacity(1 << LOG_LARGE);
        let mut j = 0;
        while j < want.len() {
            let k = want[j].0 as usize;
            let e = j + want[j..].partition_point(|w| w.0 as usize == k);
            let (s, m) = (self.frames[k].start(), self.frame_rows(k) as usize);
            self.decode_frame(k, &mut tmp[..m]);
            if (e - j) * 8 >= m {
                // many of the IDs in this frame: its (ID, row) pairs sorted once and merged with them (in ID order)
                srt.clear();
                srt.extend(tmp[..m].iter().enumerate().map(|(q, &x)| (x, q as u32)));
                srt.sort_unstable();
                let mut p = 0usize;
                for w in &want[j..e] {
                    let (i, id) = (w.1 as usize, ids[w.1 as usize]);
                    while p < m && srt[p].0 < id {
                        p += 1;
                    }
                    out[i] = if p < m && srt[p].0 == id { Some(s + srt[p].1) } else { self.ordinal_of(id, built) };
                }
            } else {
                for w in &want[j..e] {
                    let i = w.1 as usize;
                    out[i] = match tmp[..m].iter().position(|&x| x == ids[i]) {
                        Some(q) => Some(s + q as u32),
                        None => self.ordinal_of(ids[i], built),
                    };
                }
            }
            j = e;
        }
    }
}

fn get_fences(c: &mut Cur<'_>, nframes: usize) -> FormatResult<(Box<[u64]>, Box<[u64]>)> {
    check(nframes.saturating_mul(16) <= c.remaining())?;
    let (mut lo, mut hi) = (crate::try_vec(nframes, AT)?, crate::try_vec(nframes, AT)?);
    for _ in 0..nframes {
        let (a, b) = (c.u64(AT)?, c.u64(AT)?);
        check(a <= b && b < ROWID_LIMIT)?;
        lo.push(a);
        hi.push(b);
    }
    Ok((lo.into(), hi.into()))
}

impl Reverse {
    pub fn mem_bytes(&self) -> usize {
        match self {
            Reverse::Fences(x) => (x.fmin.len() + x.fmax.len() + x.smin.len()) * 8 + x.by_min.len() * 4,
            Reverse::Labels(x) => {
                x.s.mem_bytes()
                    + x.labels.len() * 8
                    + x.gfirst.len() * 4
                    + (x.fmin.len() + x.fmax.len()) * 8
                    + x.anchors.len() * 8
                    + x.aoff.len() * 4
                    + x.exc_id.len() * 12
            }
            Reverse::FrameIx(x) => x.s.mem_bytes() + x.fi.len() * 8,
            Reverse::Run(x) => x.vfirst.len() * 4 + (x.vfmin.len() + x.fmin.len() + x.fmax.len() + x.anchors.len()) * 8 + x.aoff.len() * 4,
        }
    }
}

impl Fences {
    fn new((fmin, fmax): (Box<[u64]>, Box<[u64]>)) -> Fences {
        let mut by: Vec<u32> = (0..fmin.len() as u32).collect();
        by.sort_unstable_by_key(|&k| fmin[k as usize]);
        let smin = by.iter().map(|&k| fmin[k as usize]).collect();
        let maxspan = (0..fmin.len()).map(|k| fmax[k] - fmin[k]).max().unwrap_or(0);
        Fences { fmin, fmax, by_min: by.into(), smin, maxspan }
    }

    fn lookup(&self, m: &GranMap, id: u64) -> Option<u32> {
        let mut k = self.smin.partition_point(|&x| x <= id);
        while k > 0 {
            k -= 1;
            if self.smin[k] + self.maxspan < id {
                break;
            }
            let f = self.by_min[k] as usize;
            if self.fmax[f] >= id {
                if let Some(o) = m.find_in(f, id) {
                    return Some(o);
                }
            }
        }
        None
    }
}

impl Labels {
    /// The fast path's candidate frame for `id`, or the answer itself (`Err`): an exception's ordinal, or `None` for an
    /// ID the set lacks.
    fn cand_frame(&self, id: u64) -> Result<Option<usize>, Option<u32>> {
        let Some(r) = self.s.rank(id) else { return Err(None) };
        let lab = get(&self.labels, u64::from(r) * u64::from(self.lw), self.lw);
        if lab == self.xsym {
            return Err(self.exc_id.binary_search(&id).ok().map(|i| self.exc_ord[i]));
        }
        let (fa, fb) = (self.gfirst[lab as usize] as usize, self.gfirst[lab as usize + 1] as usize);
        Ok(Some(fa + cand(&self.fmin[fa..fb], id)))
    }

    fn lookup(&self, m: &GranMap, id: u64) -> Option<u32> {
        let r = self.s.rank(id)?;
        let lab = get(&self.labels, u64::from(r) * u64::from(self.lw), self.lw);
        if lab == self.xsym {
            return self.exc_id.binary_search(&id).ok().map(|i| self.exc_ord[i]);
        }
        let (fa, fb) = (self.gfirst[lab as usize] as usize, self.gfirst[lab as usize + 1] as usize);
        let k = fa + cand(&self.fmin[fa..fb], id);
        self.find(m, k, id).or_else(|| if k > fa { self.find(m, k - 1, id) } else { None }).or_else(|| if k + 1 < fb { self.find(m, k + 1, id) } else { None })
    }

    /// `id` in frame `kk` from the anchor at or below it (or its neighbours), else the whole frame.
    fn find(&self, m: &GranMap, kk: usize, id: u64) -> Option<u32> {
        if id < self.fmin[kk] || id > self.fmax[kk] {
            return None;
        }
        find_anchored(m, kk, id, self.fmin[kk], self.aw, &self.anchors, self.aoff[kk], self.aoff[kk + 1])
    }
}

/// `id` in frame `kk` (fence minimum `fmin`, anchors `a0..a1` of `anchors` at `aw` bits) from the anchor at or below
/// it (or its neighbours), else the whole frame.
#[allow(clippy::too_many_arguments)]
fn find_anchored(m: &GranMap, kk: usize, id: u64, fmin: u64, aw: u32, anchors: &[u64], a0: u32, a1: u32) -> Option<u32> {
    let anchor = |a: u32| get(anchors, u64::from(a) * u64::from(aw), aw);
    let rel = id - fmin;
    let (mut lo, mut hi) = (a0, a1);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if anchor(mid) <= rel {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let a = lo.saturating_sub(1).max(a0);
    let rows = m.frame_rows(kk) as usize;
    let seg = |a: u32| {
        let j0 = (a - a0) as usize * ANCHOR;
        m.find_from(kk, id, j0, fmin + anchor(a), (j0 + ANCHOR).min(rows))
    };
    seg(a)
        .or_else(|| if a > a0 { seg(a - 1) } else { None })
        .or_else(|| if a + 1 < a1 { seg(a + 1) } else { None })
        .or_else(|| m.find_in(kk, id))
}

impl FrameIx {
    #[inline]
    fn frame(&self, id: u64) -> Option<usize> {
        let r = self.s.rank(id)?;
        Some(get(&self.fi, u64::from(r) * u64::from(self.w), self.w) as usize)
    }

    fn lookup(&self, m: &GranMap, id: u64) -> Option<u32> {
        m.find_in(self.frame(id)?, id)
    }
}

/// A body's elected form and its granules per form, from its header and granule table alone (`pgrc_rowid_stats`).
pub fn summary(body: &[u8]) -> FormatResult<(RevForm, [u32; 4])> {
    let mut c = Cur::new(body);
    let (version, _, rev, _) = (c.u8(AT)?, c.u8(AT)?, c.u8(AT)?, c.u8(AT)?);
    if version != VERSION {
        return Err(FormatError::BadVersion { at: AT, got: u32::from(version) });
    }
    let rev = RevForm::of(rev).ok_or_else(corrupt)?;
    let (_, ngran) = (c.u32(AT)?, c.u32(AT)? as usize);
    c.take(28, AT)?;
    let mut forms = [0u32; 4];
    for _ in 0..ngran {
        let g = Gran::get(&mut c)?;
        *forms.get_mut(g.form as usize).ok_or_else(corrupt)? += 1;
    }
    Ok((rev, forms))
}
