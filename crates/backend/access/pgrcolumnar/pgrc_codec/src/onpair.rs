//! OnPair16 string tokenization (Gargiulo & Venturini, "OnPair", arXiv 2508.02280): a pre-pass the
//! wrapper layer runs before zstd on string bytes and undoes at unwrap, so kernels only ever see the
//! original bytes.
//!
//! Ids 0..=255 are the single bytes; every later id is the concatenation of two earlier ids (the
//! pair OnPair merged), at most [`MAX_TOKEN`] bytes. A table is stored as those `(left, right)` id
//! pairs; a stream is the greedy longest-prefix parse of the input, as u16 or 12-bit symbols.
//! Training, encoding and table layout are pure functions of the input bytes.

use pgrc_format::{FormatError, FormatResult};

pub const MAX_TOKEN: usize = 16;
pub const MAX_IDS: usize = 1 << 16;
const BASE: usize = 256;

/// Stream symbol forms (the stream header's first byte).
pub const FORM_U16: u8 = 0;
pub const FORM_12: u8 = 1;
/// Stream header: `u8 form | u8 0 | u16 0 | u32 text_len | u32 symbols`.
pub const STREAM_HDR: usize = 12;
const ENCODE_PIECE: usize = 64 << 10;

#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// Most training bytes, taken as evenly spaced windows over the input.
    pub sample_max: usize,
    pub window: usize,
    /// Most ids, base bytes included (<= [`MAX_IDS`]).
    pub max_ids: usize,
    /// Pair count that merges a pair; 0 = the paper's `max(2, floor(log2(sample MiB)))`.
    pub threshold: u32,
}

impl Params {
    pub const DEFAULT: Params = Params { sample_max: 256 << 10, window: 4096, max_ids: 4096, threshold: 0 };
}

#[inline]
fn mask(len: usize) -> u64 {
    if len >= 8 { u64::MAX } else { (1u64 << (len * 8)) - 1 }
}

#[inline]
fn load16(s: &[u8], pos: usize) -> (u64, u64) {
    if let Some(w) = s.get(pos..pos + 16) {
        (u64::from_le_bytes(w[..8].try_into().expect("len 8")), u64::from_le_bytes(w[8..].try_into().expect("len 8")))
    } else {
        let mut b = [0u8; 16];
        let t = &s[pos..];
        b[..t.len()].copy_from_slice(t);
        (u64::from_le_bytes(b[..8].try_into().expect("len 8")), u64::from_le_bytes(b[8..].try_into().expect("len 8")))
    }
}

/// Open-addressed `u64 -> u32` (`u32::MAX` = empty), key and value in one slot, grown at half load.
struct Map {
    slots: Vec<(u64, u32)>,
    shift: u32,
    n: usize,
}

const NIL: u32 = u32::MAX;

impl Map {
    fn new(cap_log2: u32) -> FormatResult<Map> {
        Ok(Map { slots: pgrc_format::try_filled(1usize << cap_log2, (0u64, NIL), "onpair map")?, shift: 64 - cap_log2, n: 0 })
    }

    #[inline]
    fn get(&self, k: u64) -> u32 {
        let m = self.slots.len() - 1;
        let mut i = (k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> self.shift) as usize;
        loop {
            let (key, v) = self.slots[i];
            if v == NIL || key == k {
                return v;
            }
            i = (i + 1) & m;
        }
    }

    fn put(&mut self, k: u64, v: u32) -> FormatResult<()> {
        if (self.n + 1) * 2 > self.slots.len() {
            let mut g = Map::new(64 - self.shift + 1)?;
            for &(key, val) in &self.slots {
                if val != NIL {
                    g.put(key, val)?;
                }
            }
            *self = g;
        }
        let m = self.slots.len() - 1;
        let mut i = (k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> self.shift) as usize;
        while self.slots[i].1 != NIL && self.slots[i].0 != k {
            i = (i + 1) & m;
        }
        if self.slots[i].1 == NIL {
            self.n += 1;
        }
        self.slots[i] = (k, v);
        Ok(())
    }
}

/// A 2^16-bit presence filter over a key's hash (false positives only).
struct Filter(Vec<u64>);

impl Filter {
    fn new() -> FormatResult<Filter> {
        Ok(Filter(pgrc_format::try_filled(1 << 10, 0u64, "onpair filter")?))
    }
    #[inline]
    fn bit(k: u64) -> usize {
        (k.wrapping_mul(0xD6E8_FEB8_6659_FD93) >> 48) as usize
    }
    fn set(&mut self, k: u64) {
        let b = Filter::bit(k);
        self.0[b >> 6] |= 1 << (b & 63);
    }
    #[inline]
    fn has(&self, k: u64) -> bool {
        let b = Filter::bit(k);
        self.0[b >> 6] >> (b & 63) & 1 != 0
    }
}

/// Tokens of 4..=16 bytes in lists keyed by their first 4 (lengths 4..=8) or 8 (9..=16) bytes, longest
/// first; `val` is the token's bytes past the key, masked to its length.
#[derive(Clone, Copy)]
struct Ent {
    val: u64,
    next: u32,
    id: u16,
    len: u8,
}

struct Buckets {
    heads: Map,
    filter: Filter,
    ents: Vec<Ent>,
}

impl Buckets {
    fn new() -> FormatResult<Buckets> {
        Ok(Buckets { heads: Map::new(8)?, filter: Filter::new()?, ents: Vec::new() })
    }

    fn put(&mut self, key: u64, val: u64, len: usize, id: u16) -> FormatResult<()> {
        let e = self.ents.len() as u32;
        pgrc_format::try_reserve_more(&mut self.ents, 1, "onpair buckets")?;
        self.filter.set(key);
        let mut prev = NIL;
        let mut cur = self.heads.get(key);
        while cur != NIL && self.ents[cur as usize].len as usize >= len {
            prev = cur;
            cur = self.ents[cur as usize].next;
        }
        self.ents.push(Ent { val, next: cur, id, len: len as u8 });
        match prev {
            NIL => self.heads.put(key, e)?,
            p => self.ents[p as usize].next = e,
        }
        Ok(())
    }
}

/// The trained token set and its greedy longest-prefix matcher.
pub struct Dict {
    pairs: Vec<(u16, u16)>,
    bytes: Vec<[u8; MAX_TOKEN]>,
    lens: Vec<u8>,
    /// 2-byte tokens by their bytes (0 = none: merged ids are >= 256).
    two: Vec<u16>,
    three: Map,
    mid: Buckets,
    long: Buckets,
}

impl Dict {
    fn base() -> FormatResult<Dict> {
        let mut bytes = pgrc_format::try_vec(BASE, "onpair dict")?;
        let mut lens = pgrc_format::try_vec(BASE, "onpair dict")?;
        for b in 0..BASE {
            let mut t = [0u8; MAX_TOKEN];
            t[0] = b as u8;
            bytes.push(t);
            lens.push(1u8);
        }
        Ok(Dict {
            pairs: Vec::new(),
            bytes,
            lens,
            two: pgrc_format::try_filled(1 << 16, 0u16, "onpair dict")?,
            three: Map::new(10)?,
            mid: Buckets::new()?,
            long: Buckets::new()?,
        })
    }

    pub fn ids(&self) -> usize {
        self.bytes.len()
    }

    fn add(&mut self, l: u16, r: u16) -> FormatResult<u16> {
        let (ll, rl) = (self.lens[l as usize] as usize, self.lens[r as usize] as usize);
        let len = ll + rl;
        let mut t = self.bytes[l as usize];
        t[ll..len].copy_from_slice(&self.bytes[r as usize][..rl]);
        let id = self.bytes.len() as u16;
        pgrc_format::try_reserve_more(&mut self.bytes, 1, "onpair dict")?;
        pgrc_format::try_reserve_more(&mut self.lens, 1, "onpair dict")?;
        pgrc_format::try_reserve_more(&mut self.pairs, 1, "onpair dict")?;
        self.bytes.push(t);
        self.lens.push(len as u8);
        self.pairs.push((l, r));
        let lo = u64::from_le_bytes(t[..8].try_into().expect("len 8"));
        let hi = u64::from_le_bytes(t[8..].try_into().expect("len 8"));
        match len {
            2 => self.two[lo as usize & 0xFFFF] = id,
            3 => self.three.put(lo & mask(3), id as u32)?,
            4..=8 => self.mid.put(lo & 0xFFFF_FFFF, (lo & mask(len)) >> 32, len, id)?,
            _ => self.long.put(lo, hi & mask(len - 8), len, id)?,
        }
        Ok(id)
    }

    /// The longest token that prefixes `s[pos..]` (`pos < s.len()`).
    #[inline]
    pub fn longest(&self, s: &[u8], pos: usize) -> (u16, usize) {
        let rem = s.len() - pos;
        if rem < 2 {
            return (s[pos] as u16, 1);
        }
        let (lo, hi) = load16(s, pos);
        if rem > 8 && self.long.filter.has(lo) {
            let mut e = self.long.heads.get(lo);
            while e != NIL {
                let t = self.long.ents[e as usize];
                let l = t.len as usize;
                if l <= rem && hi & mask(l - 8) == t.val {
                    return (t.id, l);
                }
                e = t.next;
            }
        }
        if rem > 3 && self.mid.filter.has(lo & 0xFFFF_FFFF) {
            let mut e = self.mid.heads.get(lo & 0xFFFF_FFFF);
            while e != NIL {
                let t = self.mid.ents[e as usize];
                let l = t.len as usize;
                if l <= rem && (lo & mask(l)) >> 32 == t.val {
                    return (t.id, l);
                }
                e = t.next;
            }
        }
        if rem > 2 {
            let id = self.three.get(lo & mask(3));
            if id != NIL {
                return (id as u16, 3);
            }
        }
        match self.two[lo as usize & 0xFFFF] {
            0 => (s[pos] as u16, 1),
            id => (id, 2),
        }
    }

    /// OnPair training over evenly spaced windows of `text`: greedy parse with the growing set, count
    /// each adjacent pair, merge a pair the moment its count reaches the threshold (the merged token
    /// then stands for the pair's left side).
    pub fn train(text: &[u8], p: &Params) -> FormatResult<Dict> {
        let mut d = Dict::base()?;
        let max_ids = p.max_ids.clamp(BASE, MAX_IDS);
        if text.len() < 2 || max_ids == BASE {
            return Ok(d);
        }
        let window = p.window.max(64);
        let take = text.len().min(p.sample_max.max(window));
        let nwin = take.div_ceil(window);
        let stride = if nwin > 1 { (text.len() - window.min(text.len())) / (nwin - 1) } else { 0 };
        let threshold = match p.threshold {
            0 => (take >> 20).max(1).ilog2().max(2),
            t => t.max(1),
        };
        let mut counts = PairCounts::new(take)?;
        // the token arrays at their final size up front: no doubling, no copy window past the set's own bytes
        let more = max_ids - BASE;
        let oom = |_| FormatError::OutOfMemory { bytes: more * (MAX_TOKEN + 5) };
        d.bytes.try_reserve_exact(more).map_err(oom)?;
        d.lens.try_reserve_exact(more).map_err(oom)?;
        d.pairs.try_reserve_exact(more).map_err(oom)?;
        for w in 0..nwin {
            let start = w * stride;
            let s = &text[start..(start + window).min(text.len())];
            let mut pos = 0;
            let mut prev: Option<u16> = None;
            while pos < s.len() {
                let (id, len) = d.longest(s, pos);
                pos += len;
                let cur = match prev {
                    Some(l) if d.lens[l as usize] as usize + len <= MAX_TOKEN && d.ids() < max_ids => {
                        if counts.bump(l, id)? >= threshold { d.add(l, id)? } else { id }
                    }
                    _ => id,
                };
                prev = Some(cur);
            }
        }
        Ok(d)
    }

    /// `u16 merged | merged x (u16 left, u16 right)`.
    pub fn write_table(&self, out: &mut Vec<u8>) -> FormatResult<()> {
        pgrc_format::try_reserve_more(out, 2 + self.pairs.len() * 4, "onpair table")?;
        out.extend_from_slice(&(self.pairs.len() as u16).to_le_bytes());
        for &(l, r) in &self.pairs {
            out.extend_from_slice(&l.to_le_bytes());
            out.extend_from_slice(&r.to_le_bytes());
        }
        Ok(())
    }

    /// The symbol form a stream over this set uses: 12-bit when every id fits.
    pub fn form(&self) -> u8 {
        if self.ids() <= 1 << 12 { FORM_12 } else { FORM_U16 }
    }

    /// The stream bytes [`Self::encode`] needs for `text` at most, estimated from a parse of its first 64 KiB
    /// (with room for one piece more); exact for a text that short.
    pub fn stream_bound(&self, text: &[u8]) -> usize {
        let head = &text[..text.len().min(ENCODE_PIECE)];
        let (mut pos, mut n) = (0usize, 0usize);
        while pos < head.len() {
            pos += self.longest(head, pos).1;
            n += 1;
        }
        let est = (n as u128 * text.len() as u128).div_ceil(head.len().max(1) as u128) as usize;
        STREAM_HDR + 2 * est + est / 16 + 2 * ENCODE_PIECE + 4
    }

    /// Greedy longest-prefix parse of `text`, appended to `out` as one stream (header + symbols); `out` grows a
    /// piece of `text` at a time, so it holds about the stream, not twice the text.
    pub fn encode(&self, text: &[u8], form: u8, out: &mut Vec<u8>) -> FormatResult<()> {
        const PIECE: usize = ENCODE_PIECE;
        if form == FORM_12 && self.ids() > 1 << 12 {
            return Err(FormatError::EncodeContract { detail: "onpair 12-bit form over a wide set" });
        }
        let start = out.len();
        pgrc_format::try_resize(out, start + STREAM_HDR, 0, "onpair stream")?;
        out[start] = form;
        out[start + 4..start + 8].copy_from_slice(&(text.len() as u32).to_le_bytes());
        let (mut n, mut w, mut pos, mut half) = (0usize, start + STREAM_HDR, 0usize, 0u32);
        while pos < text.len() {
            let end = (pos + PIECE).min(text.len());
            pgrc_format::try_resize(out, w + 2 * (end - pos) + 4, 0, "onpair stream")?;
            while pos < end {
                let (id, len) = self.longest(text, pos);
                pos += len;
                if form == FORM_U16 {
                    out[w..w + 2].copy_from_slice(&id.to_le_bytes());
                    w += 2;
                } else if n & 1 == 0 {
                    half = id as u32;
                } else {
                    out[w..w + 4].copy_from_slice(&(half | (id as u32) << 12).to_le_bytes());
                    w += 3;
                }
                n += 1;
            }
            out.truncate(w);
        }
        if form == FORM_12 && n & 1 == 1 {
            pgrc_format::try_resize(out, w + 4, 0, "onpair stream")?;
            out[w..w + 4].copy_from_slice(&half.to_le_bytes());
            w += 3;
        }
        out.truncate(w);
        out[start + 8..start + 12].copy_from_slice(&(n as u32).to_le_bytes());
        Ok(())
    }
}

/// Open-addressed pair counts; a full table stops counting new pairs (known pairs keep counting).
struct PairCounts {
    keys: Vec<u32>,
    counts: Vec<u32>,
    shift: u32,
    n: usize,
}

impl PairCounts {
    fn new(sample: usize) -> FormatResult<PairCounts> {
        let log2 = (sample.max(1 << 10).next_power_of_two().ilog2() + 1).min(18);
        Ok(PairCounts {
            keys: pgrc_format::try_filled(1 << log2, 0u32, "onpair pairs")?,
            counts: pgrc_format::try_filled(1 << log2, 0u32, "onpair pairs")?,
            shift: 32 - log2,
            n: 0,
        })
    }

    #[inline]
    fn bump(&mut self, l: u16, r: u16) -> FormatResult<u32> {
        let k = (l as u32) << 16 | r as u32;
        let m = self.keys.len() - 1;
        let mut i = (k.wrapping_mul(0x9E37_79B9) >> self.shift) as usize;
        loop {
            if self.counts[i] == 0 {
                if (self.n + 1) * 4 > self.keys.len() * 3 {
                    return Ok(0);
                }
                self.keys[i] = k;
                self.counts[i] = 1;
                self.n += 1;
                return Ok(1);
            }
            if self.keys[i] == k {
                self.counts[i] += 1;
                return Ok(self.counts[i]);
            }
            i = (i + 1) & m;
        }
    }
}

/// A table expanded for decode: every id's bytes in a 16-byte slot, for one unconditional copy each.
pub struct DecodeTable {
    slots: Vec<[u8; MAX_TOKEN]>,
    lens: Vec<u8>,
}

impl DecodeTable {
    pub fn ids(&self) -> usize {
        self.slots.len()
    }

    /// The bytes it holds.
    pub fn bytes(&self) -> usize {
        self.slots.capacity() * MAX_TOKEN + self.lens.capacity()
    }

    /// Parse a table written by [`Dict::write_table`]; returns the table and the bytes it took.
    pub fn read(b: &[u8]) -> FormatResult<(DecodeTable, usize)> {
        let at = "onpair table";
        let m = u16::from_le_bytes(b.get(..2).ok_or(FormatError::Truncated { at })?.try_into().expect("len 2")) as usize;
        let need = 2 + m * 4;
        let body = b.get(2..need).ok_or(FormatError::Truncated { at })?;
        let n = BASE + m;
        if n > MAX_IDS {
            return Err(FormatError::Corrupt { at });
        }
        let mut slots = pgrc_format::try_vec(n, at)?;
        let mut lens = pgrc_format::try_vec(n, at)?;
        for i in 0..BASE {
            let mut t = [0u8; MAX_TOKEN];
            t[0] = i as u8;
            slots.push(t);
            lens.push(1u8);
        }
        for (k, p) in body.chunks_exact(4).enumerate() {
            let id = BASE + k;
            let l = u16::from_le_bytes([p[0], p[1]]) as usize;
            let r = u16::from_le_bytes([p[2], p[3]]) as usize;
            if l >= id || r >= id {
                return Err(FormatError::Corrupt { at });
            }
            let (ll, rl) = (lens[l] as usize, lens[r] as usize);
            if ll + rl > MAX_TOKEN {
                return Err(FormatError::Corrupt { at });
            }
            let mut t: [u8; MAX_TOKEN] = slots[l];
            let rs: [u8; MAX_TOKEN] = slots[r];
            t[ll..ll + rl].copy_from_slice(&rs[..rl]);
            slots.push(t);
            lens.push((ll + rl) as u8);
        }
        Ok((DecodeTable { slots, lens }, need))
    }
}

/// A stream's `(form, text_len, symbols, symbol bytes)` from its header; the stream is `b[..STREAM_HDR + bytes]`.
pub fn stream_header(b: &[u8]) -> FormatResult<(u8, usize, usize, usize)> {
    let at = "onpair stream";
    let h = b.get(..STREAM_HDR).ok_or(FormatError::Truncated { at })?;
    let form = h[0];
    let text_len = u32::from_le_bytes(h[4..8].try_into().expect("len 4")) as usize;
    let n = u32::from_le_bytes(h[8..12].try_into().expect("len 4")) as usize;
    let bytes = match form {
        FORM_U16 => n.checked_mul(2),
        FORM_12 => n.div_ceil(2).checked_mul(3),
        _ => None,
    }
    .ok_or(FormatError::Corrupt { at })?;
    if h[1..4] != [0, 0, 0] || n > text_len || (text_len > 0 && n == 0) {
        return Err(FormatError::Corrupt { at });
    }
    Ok((form, text_len, n, bytes))
}

/// Decode one stream (at the start of `b`) into exactly `dst`; returns the bytes the stream took.
pub fn decode(t: &DecodeTable, b: &[u8], dst: &mut [u8]) -> FormatResult<usize> {
    let at = "onpair stream";
    let (form, text_len, n, bytes) = stream_header(b)?;
    if text_len != dst.len() {
        return Err(FormatError::Corrupt { at });
    }
    let syms = b.get(STREAM_HDR..STREAM_HDR + bytes).ok_or(FormatError::Truncated { at })?;
    let pos = match form {
        FORM_U16 => decode_syms(t, syms.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])), dst)?,
        _ => {
            let it = syms.chunks_exact(3).flat_map(|c| {
                let v = c[0] as u32 | (c[1] as u32) << 8 | (c[2] as u32) << 16;
                [(v & 0xFFF) as u16, (v >> 12) as u16]
            });
            decode_syms(t, it.take(n), dst)?
        }
    };
    if pos != dst.len() {
        return Err(FormatError::Corrupt { at });
    }
    Ok(STREAM_HDR + bytes)
}

#[inline(always)]
fn decode_syms(t: &DecodeTable, syms: impl Iterator<Item = u16>, dst: &mut [u8]) -> FormatResult<usize> {
    let at = "onpair stream";
    let (slots, lens) = (&t.slots[..], &t.lens[..t.slots.len()]);
    let end = dst.len();
    let mut pos = 0usize;
    for s in syms {
        let s = s as usize;
        let (Some(slot), Some(&len)) = (slots.get(s), lens.get(s)) else {
            return Err(FormatError::Corrupt { at });
        };
        let len = len as usize;
        if let Some(w) = dst.get_mut(pos..pos + MAX_TOKEN) {
            w.copy_from_slice(slot);
        } else {
            dst.get_mut(pos..pos + len).ok_or(FormatError::Corrupt { at })?.copy_from_slice(&slot[..len]);
        }
        pos += len;
    }
    if pos > end {
        return Err(FormatError::Corrupt { at });
    }
    Ok(pos)
}
