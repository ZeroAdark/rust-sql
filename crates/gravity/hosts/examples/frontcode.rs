//! Front coding of the pgrc dictionary payload, measured over a real table's parts (lane
//! frontcode, `docs/gravity/frontcode-measurement.md`). No format change: the front-coded image is
//! built in memory per part (restart every k-th entry stored whole as `u32 len + bytes`, the others
//! `varint(shared prefix len) + suffix`; byte lengths stay in the 12 B index) and measured against
//! today's contiguous payload.
//!
//!   frontcode <table dir> census                         part 0's dictionary-coded columns
//!   frontcode <table dir> wrappers <attnos csv>          the wrapper byte and extent count of every part's code / index / payload streams
//!   frontcode <table dir> bytes <attnos csv> [w]         bytes per column: today / front-coded at k, prefix distribution, zstd
//!   frontcode <table dir> cpu <attno> <needle> [w]       ns per entry: scan, fingerprint, render by code, prefix search
//!   frontcode <table dir> expand <attno> [w]             the fault path: zstd unwrap per frame + expansion to the raw image, ms per part
//!   frontcode <table dir> index <attnos csv> [w]         the DictIndex packed (anchors + bit-packed lengths + char deltas) vs 12 B records, expand cost
//!   frontcode <table dir> resident <attnos csv> [w]      lane dictresident: under the process's PGRUST_PGRC_DICT_RESIDENT arm, per column the resident
//!                                                        payload + index bytes, the fault path ms per part, ns per random entry (first touch / memo hit)
//!                                                        and per walked entry, the LIKE table build per part

use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use pgrc_format::enc::EncodingId;
use pgrc_format::part::StreamRole;
use pgrc_read::{DictHandle, MemPartIo, OpenPart, PartExpect};

const KS: [usize; 4] = [16, 32, 64, 128];
const FRAME: usize = 1024;
const HIST: [usize; 9] = [0, 1, 4, 8, 16, 32, 64, 128, 256];

fn open_parts(dir: &str) -> Vec<Arc<OpenPart>> {
    let mut files: Vec<_> = std::fs::read_dir(dir).expect("table dir").filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("part-") && n.to_string_lossy().ends_with(".pgrc"))).collect();
    files.sort();
    files.iter().enumerate().map(|(i, p)| Arc::new(OpenPart::open(Box::new(MemPartIo::new(std::fs::read(p).expect("part file"), 7, 1000 + i as u64)), &PartExpect::none()).expect("open part"))).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).expect("table dir");
    let parts = open_parts(dir);
    match args.get(2).map(String::as_str) {
        Some("census") => census(&parts[0]),
        Some("wrappers") => {
            let attnos: Vec<u32> = args.get(3).expect("attnos").split(',').filter_map(|a| a.parse().ok()).collect();
            let mut hist = std::collections::BTreeMap::new();
            for part in &parts {
                let dir = part.stream_directory().expect("dir");
                for &attno in &attnos {
                    for role in [StreamRole::Values, StreamRole::DictIndex, StreamRole::DictPayload] {
                        if let Some(s) = dir.lookup(attno, 0, role) {
                            *hist.entry((attno, role as u8, s.entry.wrapper, s.extents.len())).or_insert(0u32) += 1;
                        }
                    }
                }
            }
            for ((attno, role, wrapper, extents), n) in hist {
                println!("attno {attno} role {role} wrapper {wrapper} extents {extents}: {n} parts");
            }
        }
        Some("bytes") => {
            let attnos: Vec<u32> = args.get(3).expect("attnos").split(',').filter_map(|a| a.parse().ok()).collect();
            let w: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(16);
            bytes(&parts, &attnos, w);
        }
        Some("cpu") => {
            let attno: u32 = args.get(3).expect("attno").parse().unwrap();
            let needle = args.get(4).expect("needle").clone().into_bytes();
            let w: usize = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(16);
            cpu(&parts, attno, &needle, w);
        }
        Some("expand") => {
            let attno: u32 = args.get(3).expect("attno").parse().unwrap();
            let w: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1);
            expand(&parts, attno, w);
        }
        Some("index") => {
            let attnos: Vec<u32> = args.get(3).expect("attnos").split(',').filter_map(|a| a.parse().ok()).collect();
            let w: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1);
            index(&parts, &attnos, w);
        }
        Some("resident") => {
            let attnos: Vec<u32> = args.get(3).expect("attnos").split(',').filter_map(|a| a.parse().ok()).collect();
            let w: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1);
            resident(&parts, &attnos, w);
        }
        _ => panic!("mode: census | bytes | cpu | expand | index | resident"),
    }
}

fn census(part: &Arc<OpenPart>) {
    let dir = part.stream_directory().expect("dir");
    for s in dir.streams() {
        if s.role == StreamRole::Values && s.entry.encoding == EncodingId::DictCodes as u16 {
            let d = DictHandle::open(Arc::clone(part), None, pgrc_am::scan::codec_binding().unwrappers, s.entry.attno, s.entry.path_ord).expect("dict");
            println!("attno {} path {} class {} fixed_len {} ncodes {}", s.entry.attno, s.entry.path_ord, s.entry.class, s.entry.fixed_len, d.ncodes());
        }
    }
}

struct Spans<'a> {
    index: &'a [u8],
}

impl Spans<'_> {
    #[inline(always)]
    fn span(&self, code: u32) -> (usize, usize) {
        let at = code as usize * 12;
        (u32::from_le_bytes(self.index[at..at + 4].try_into().unwrap()) as usize + 4, u32::from_le_bytes(self.index[at + 4..at + 8].try_into().unwrap()) as usize)
    }
    fn n(&self) -> u32 {
        (self.index.len() / 12) as u32
    }
}

impl gravity_dict::EntryIndex for Spans<'_> {
    fn ncodes(&self) -> u32 {
        self.n()
    }
    fn span(&self, code: u32) -> (usize, usize) {
        Spans::span(self, code)
    }
}

#[inline(always)]
fn lcp(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + 8 <= n {
        let x = u64::from_le_bytes(a[i..i + 8].try_into().unwrap()) ^ u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        if x != 0 {
            return i + (x.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

fn varint_len(v: usize) -> usize {
    let mut n = 1;
    let mut v = v >> 7;
    while v != 0 {
        n += 1;
        v >>= 7;
    }
    n
}

fn put_varint(out: &mut Vec<u8>, mut v: usize) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

#[inline(always)]
fn get_varint(img: &[u8], pos: &mut usize) -> usize {
    let (mut v, mut shift) = (0usize, 0);
    loop {
        let b = img[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as usize) << shift;
        if b < 0x80 {
            return v;
        }
        shift += 7;
    }
}

/// The front-coded image of a part at restart interval `k`, with each entry's start offset.
struct Fc {
    k: usize,
    img: Vec<u8>,
    off: Vec<u32>,
    len: Vec<u32>,
}

impl Fc {
    fn build(payload: &[u8], sp: &Spans, k: usize) -> Fc {
        let n = sp.n() as usize;
        let (mut img, mut off, mut len) = (Vec::with_capacity(payload.len()), Vec::with_capacity(n), Vec::with_capacity(n));
        let mut prev: &[u8] = &[];
        for c in 0..n {
            let (s, l) = sp.span(c as u32);
            let e = &payload[s..s + l];
            off.push(img.len() as u32);
            len.push(l as u32);
            if c % k == 0 {
                img.extend_from_slice(&(l as u32).to_le_bytes());
                img.extend_from_slice(e);
            } else {
                let p = lcp(prev, e);
                put_varint(&mut img, p);
                img.extend_from_slice(&e[p..]);
            }
            prev = e;
        }
        Fc { k, img, off, len }
    }

    /// Decode entry `c` at image position `pos` onto `buf` (the previous entry when `c % k != 0`); returns the prefix length.
    #[inline(always)]
    fn step(&self, c: usize, pos: &mut usize, buf: &mut [u8]) -> usize {
        let l = self.len[c] as usize;
        if c % self.k == 0 {
            *pos += 4;
            buf[..l].copy_from_slice(&self.img[*pos..*pos + l]);
            *pos += l;
            0
        } else {
            let p = get_varint(&self.img, pos);
            buf[p..l].copy_from_slice(&self.img[*pos..*pos + l - p]);
            *pos += l - p;
            p
        }
    }

    /// Entry `c` decoded from its restart point into `buf`.
    #[inline(always)]
    fn render(&self, c: usize, buf: &mut [u8]) -> usize {
        let r = c - c % self.k;
        let mut pos = self.off[r] as usize;
        for i in r..=c {
            self.step(i, &mut pos, buf);
        }
        self.len[c] as usize
    }
}

#[derive(Default, Clone)]
struct ColStats {
    parts: u64,
    verbatim: u64,
    entries: u64,
    payload: u64,
    entry_bytes: u64,
    lcp_sum: u64,
    hist: [u64; 9],
    fc: [u64; 4],
    zstd_raw: u64,
    zstd_fc: [u64; 4],
    zstd_raw_frames: u64,
    zstd_fc_frames: [u64; 4],
    disk_codes: u64,
    disk_index: u64,
    disk_payload: u64,
    disk_verbatim: u64,
}

fn bytes(parts: &[Arc<OpenPart>], attnos: &[u32], w: usize) {
    let binding = pgrc_am::scan::codec_binding();
    let stats: Vec<Mutex<ColStats>> = attnos.iter().map(|_| Mutex::new(ColStats::default())).collect();
    let next = AtomicUsize::new(0);
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..w {
            s.spawn(|| loop {
                let pi = next.fetch_add(1, Ordering::Relaxed);
                let Some(part) = parts.get(pi) else { break };
                let dir = part.stream_directory().expect("dir");
                for (ai, &attno) in attnos.iter().enumerate() {
                    let mut st = ColStats::default();
                    st.parts = 1;
                    let values = dir.lookup(attno, 0, StreamRole::Values).expect("values stream");
                    let disk = |s: &pgrc_read::streams::ParsedStream| s.extents.iter().map(|e| e.len).sum::<u64>();
                    if values.entry.encoding != EncodingId::DictCodes as u16 {
                        st.verbatim = 1;
                        st.disk_verbatim = disk(values);
                        let mut g = stats[ai].lock().unwrap();
                        add(&mut g, &st);
                        continue;
                    }
                    st.disk_codes = disk(values);
                    st.disk_index = disk(dir.lookup(attno, 0, StreamRole::DictIndex).expect("index stream"));
                    st.disk_payload = disk(dir.lookup(attno, 0, StreamRole::DictPayload).expect("payload stream"));
                    let d = DictHandle::open(Arc::clone(part), None, binding.unwrappers, attno, 0).expect("dict");
                    d.prewarm_payload().expect("prewarm");
                    let payload = d.payload_bytes().expect("payload").expect("whole arm");
                    let sp = Spans { index: d.index_bytes().expect("index") };
                    let n = sp.n() as usize;
                    st.entries = n as u64;
                    st.payload = payload.len() as u64;
                    let mut prev: &[u8] = &[];
                    for c in 0..n {
                        let (s, l) = sp.span(c as u32);
                        let e = &payload[s..s + l];
                        let p = lcp(prev, e);
                        st.entry_bytes += l as u64;
                        st.lcp_sum += p as u64;
                        st.hist[HIST.iter().rposition(|&h| p >= h).unwrap()] += 1;
                        for (ki, &k) in KS.iter().enumerate() {
                            st.fc[ki] += if c % k == 0 { 4 + l as u64 } else { (varint_len(p) + l - p) as u64 };
                        }
                        prev = e;
                    }
                    st.zstd_raw = zstd::bulk::compress(payload, 3).expect("zstd").len() as u64;
                    let frame_start = |c: usize| if c >= n { payload.len() } else { sp.span(c as u32).0 - 4 };
                    st.zstd_raw_frames = (0..n.div_ceil(FRAME)).map(|f| zstd::bulk::compress(&payload[frame_start(f * FRAME)..frame_start((f + 1) * FRAME)], 3).expect("zstd").len() as u64).sum();
                    for (ki, &k) in KS.iter().enumerate() {
                        let fc = Fc::build(payload, &sp, k);
                        st.zstd_fc[ki] = zstd::bulk::compress(&fc.img, 3).expect("zstd").len() as u64;
                        let fstart = |c: usize| if c >= n { fc.img.len() } else { fc.off[c] as usize };
                        st.zstd_fc_frames[ki] = (0..n.div_ceil(FRAME)).map(|f| zstd::bulk::compress(&fc.img[fstart(f * FRAME)..fstart((f + 1) * FRAME)], 3).expect("zstd").len() as u64).sum();
                    }
                    let mut g = stats[ai].lock().unwrap();
                    add(&mut g, &st);
                }
            });
        }
    });
    eprintln!("bytes: {} parts, {} columns, wall {:.1} s", parts.len(), attnos.len(), t0.elapsed().as_secs_f64());
    let mb = |b: u64| b as f64 / 1e6;
    for (ai, &attno) in attnos.iter().enumerate() {
        let s = stats[ai].lock().unwrap().clone();
        println!("attno {attno}: {} dict parts ({} verbatim, {:.1} MB on disk), {} entries, mean entry {:.1} B, mean prefix {:.1} B ({:.1} %)", s.parts - s.verbatim, s.verbatim, mb(s.disk_verbatim), s.entries, s.entry_bytes as f64 / s.entries as f64, s.lcp_sum as f64 / s.entries as f64, 100.0 * s.lcp_sum as f64 / s.entry_bytes as f64);
        println!("  today: payload image {:.1} MB (entry bytes {:.1} MB + headers/padding), index {:.1} MB, on disk: codes {:.1} MB, index {:.1} MB, payload {:.1} MB (zstd-3 of the image: whole {:.1} MB, one block per 1,024-entry frame {:.1} MB)", mb(s.payload), mb(s.entry_bytes), mb(s.entries * 12), mb(s.disk_codes), mb(s.disk_index), mb(s.disk_payload), mb(s.zstd_raw), mb(s.zstd_raw_frames));
        println!("  prefix histogram (lower bounds {:?}): {:?}", HIST, s.hist);
        for (ki, &k) in KS.iter().enumerate() {
            println!("  k={k}: front-coded image {:.1} MB (ratio {:.3}), +1 B/entry {:.1} MB ({:.3}); zstd-3 whole {:.1} MB ({:.3} of zstd raw), per frame {:.1} MB ({:.3}); restart-only index {:.1} MB", mb(s.fc[ki]), s.fc[ki] as f64 / s.payload as f64, mb(s.fc[ki] + s.entries), (s.fc[ki] + s.entries) as f64 / s.payload as f64, mb(s.zstd_fc[ki]), s.zstd_fc[ki] as f64 / s.zstd_raw as f64, mb(s.zstd_fc_frames[ki]), s.zstd_fc_frames[ki] as f64 / s.zstd_raw_frames as f64, mb(s.entries.div_ceil(k as u64) * 12));
        }
    }
}

fn add(g: &mut ColStats, s: &ColStats) {
    g.parts += s.parts;
    g.verbatim += s.verbatim;
    g.entries += s.entries;
    g.payload += s.payload;
    g.entry_bytes += s.entry_bytes;
    g.lcp_sum += s.lcp_sum;
    g.zstd_raw += s.zstd_raw;
    g.zstd_raw_frames += s.zstd_raw_frames;
    g.disk_codes += s.disk_codes;
    g.disk_index += s.disk_index;
    g.disk_payload += s.disk_payload;
    g.disk_verbatim += s.disk_verbatim;
    for i in 0..9 {
        g.hist[i] += s.hist[i];
    }
    for i in 0..4 {
        g.fc[i] += s.fc[i];
        g.zstd_fc[i] += s.zstd_fc[i];
        g.zstd_fc_frames[i] += s.zstd_fc_frames[i];
    }
}

#[derive(Default, Clone, Copy)]
struct Cpu {
    entries: u64,
    scan_payload: u64,
    scan_entry: u64,
    scan_fc: [u64; 4],
    scan_fc_smart: [u64; 4],
    decode_fc: [u64; 4],
    mismatch: u64,
    fp_raw: u64,
    fp_fc: [u64; 4],
    renders: u64,
    render_raw: u64,
    render_fc: [u64; 4],
    searches: u64,
    search_raw: u64,
    search_fc: [u64; 4],
    hits: u64,
}

fn lower_bound(n: usize, mut less: impl FnMut(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if less(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

fn starts_with_or_less(e: &[u8], x: &[u8]) -> bool {
    let m = e.len().min(x.len());
    e[..m] <= x[..m]
}

fn cpu(parts: &[Arc<OpenPart>], attno: u32, needle: &[u8], w: usize) {
    let binding = pgrc_am::scan::codec_binding();
    let total = Mutex::new(Cpu::default());
    let next = AtomicUsize::new(0);
    let pair = gravity_dict::substr::RarePair::new(needle).expect("needle");
    let (k0, k1) = (0x0706_0504_0302_0100u64, 0x0f0e_0d0c_0b0a_0908u64);
    let pattern: Vec<u8> = [b"%", needle, b"%"].concat();
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..w {
            s.spawn(|| {
                let mut region_buf = vec![0u64; 1 << 16];
                let mut buf = vec![0u8; 1 << 16];
                loop {
                    let pi = next.fetch_add(1, Ordering::Relaxed);
                    let Some(part) = parts.get(pi) else { break };
                    let dir = part.stream_directory().expect("dir");
                    if dir.lookup(attno, 0, StreamRole::Values).expect("values").entry.encoding != EncodingId::DictCodes as u16 {
                        continue;
                    }
                    let d = DictHandle::open(Arc::clone(part), None, binding.unwrappers, attno, 0).expect("dict");
                    d.prewarm_payload().expect("prewarm");
                    let payload = d.payload_bytes().expect("payload").expect("whole arm");
                    let sp = Spans { index: d.index_bytes().expect("index") };
                    let n = sp.n() as usize;
                    let fcs: Vec<Fc> = KS.iter().map(|&k| Fc::build(payload, &sp, k)).collect();
                    let mut c = Cpu { entries: n as u64, ..Cpu::default() };
                    let t = Instant::now();
                    let mut region = unsafe { gravity_dict::Region::new(region_buf.as_mut_ptr() as *mut u8, region_buf.len() * 8) };
                    let m = gravity_dict::Matcher::new(&pattern, gravity_dict::Mode::Like, &mut region).unwrap();
                    let bits = m.build_bits_over(&sp, payload, &mut region).unwrap_or_else(|e| panic!("region: {e:?}"));
                    let hits = (0..n as u32).filter(|&c| unsafe { bits.get(c) }).count() as u64;
                    c.scan_payload = t.elapsed().as_nanos() as u64;
                    c.hits = hits;
                    let t = Instant::now();
                    let mut h = 0u64;
                    for i in 0..n as u32 {
                        let (s, l) = sp.span(i);
                        h += pair.contains(needle, &payload[s..s + l]) as u64;
                    }
                    c.scan_entry = t.elapsed().as_nanos() as u64;
                    assert_eq!(h, hits);
                    let nbits = unsafe { gravity_dict::Bits::at(m.negated().build_bits_over(&sp, payload, &mut region).unwrap_or_else(|e| panic!("region: {e:?}")).header()) };
                    let mut words = vec![0u64; n.div_ceil(64)];
                    for (ki, fc) in fcs.iter().enumerate() {
                        let t = Instant::now();
                        let mut pos = 0usize;
                        let mut sum = 0u64;
                        for i in 0..n {
                            fc.step(i, &mut pos, &mut buf);
                            sum += buf[fc.len[i] as usize / 2] as u64;
                        }
                        c.decode_fc[ki] = t.elapsed().as_nanos() as u64;
                        std::hint::black_box(sum);
                        let t = Instant::now();
                        let (mut pos, mut h) = (0usize, 0u64);
                        for i in 0..n {
                            fc.step(i, &mut pos, &mut buf);
                            h += pair.contains(needle, &buf[..fc.len[i] as usize]) as u64;
                        }
                        c.scan_fc[ki] = t.elapsed().as_nanos() as u64;
                        assert_eq!(h, hits, "k={}", fc.k);
                        words.fill(0);
                        let t = Instant::now();
                        let (mut pos, mut prev_end) = (0usize, usize::MAX);
                        for i in 0..n {
                            let p = fc.step(i, &mut pos, &mut buf);
                            let l = fc.len[i] as usize;
                            if p < prev_end {
                                prev_end = pair.find(needle, &buf[..l], p.saturating_sub(needle.len() - 1)).map_or(usize::MAX, |at| at + needle.len());
                            }
                            words[i / 64] |= ((prev_end != usize::MAX) as u64) << (i % 64);
                        }
                        c.scan_fc_smart[ki] = t.elapsed().as_nanos() as u64;
                        for i in 0..n as u32 {
                            let b = words[i as usize / 64] >> (i % 64) & 1 != 0;
                            c.mismatch += (b != unsafe { bits.get(i) }) as u64 + (!b != unsafe { nbits.get(i) }) as u64;
                        }
                    }
                    let t = Instant::now();
                    let mut acc = [0u64; 2];
                    for i in 0..n as u32 {
                        let (s, l) = sp.span(i);
                        let f = gravity_dict::fp::siphash128(k0, k1, &payload[s..s + l]);
                        acc[0] ^= f[0];
                        acc[1] ^= f[1];
                    }
                    c.fp_raw = t.elapsed().as_nanos() as u64;
                    for (ki, fc) in fcs.iter().enumerate() {
                        let t = Instant::now();
                        let (mut pos, mut a) = (0usize, [0u64; 2]);
                        for i in 0..n {
                            fc.step(i, &mut pos, &mut buf);
                            let f = gravity_dict::fp::siphash128(k0, k1, &buf[..fc.len[i] as usize]);
                            a[0] ^= f[0];
                            a[1] ^= f[1];
                        }
                        c.fp_fc[ki] = t.elapsed().as_nanos() as u64;
                        assert_eq!(a, acc);
                    }
                    let renders = 100_000usize;
                    let mut rng = 0x9E37_79B9_7F4A_7C15u64 ^ pi as u64;
                    let codes: Vec<usize> = (0..renders).map(|_| {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        (rng % n as u64) as usize
                    }).collect();
                    c.renders = renders as u64;
                    let t = Instant::now();
                    let mut sum = 0u64;
                    for &code in &codes {
                        let (s, l) = sp.span(code as u32);
                        let e = &payload[s..s + l];
                        sum += if l > 0 { e[0] as u64 + e[l - 1] as u64 } else { 0 };
                    }
                    c.render_raw = t.elapsed().as_nanos() as u64;
                    for (ki, fc) in fcs.iter().enumerate() {
                        let t = Instant::now();
                        let mut s2 = 0u64;
                        for &code in &codes {
                            let l = fc.render(code, &mut buf);
                            s2 += if l > 0 { buf[0] as u64 + buf[l - 1] as u64 } else { 0 };
                        }
                        c.render_fc[ki] = t.elapsed().as_nanos() as u64;
                        assert_eq!(s2, sum);
                    }
                    let searches = 2_000usize;
                    let prefixes: Vec<Vec<u8>> = codes[..searches].iter().map(|&code| {
                        let (s, l) = sp.span(code as u32);
                        payload[s..s + l.min(12)].to_vec()
                    }).collect();
                    c.searches = searches as u64;
                    let t = Instant::now();
                    let mut bounds = Vec::with_capacity(searches);
                    for x in &prefixes {
                        let entry = |i: usize| { let (s, l) = sp.span(i as u32); &payload[s..s + l] };
                        let lo = lower_bound(n, |i| entry(i) < &x[..]);
                        let hi = lower_bound(n, |i| starts_with_or_less(entry(i), x));
                        bounds.push((lo, hi));
                    }
                    c.search_raw = t.elapsed().as_nanos() as u64;
                    for (ki, fc) in fcs.iter().enumerate() {
                        let t = Instant::now();
                        let nr = n.div_ceil(fc.k);
                        for (x, &(lo, hi)) in prefixes.iter().zip(&bounds) {
                            let restart = |r: usize| { let o = fc.off[r * fc.k] as usize + 4; &fc.img[o..o + fc.len[r * fc.k] as usize] };
                            let mut find = |less: &mut dyn FnMut(&[u8]) -> bool| {
                                let r = lower_bound(nr, |r| less(restart(r)));
                                if r == 0 {
                                    return 0;
                                }
                                let start = (r - 1) * fc.k;
                                let mut pos = fc.off[start] as usize;
                                for i in start..(start + fc.k).min(n) {
                                    fc.step(i, &mut pos, &mut buf);
                                    if !less(&buf[..fc.len[i] as usize]) {
                                        return i;
                                    }
                                }
                                (start + fc.k).min(n)
                            };
                            let flo = find(&mut |e| e < &x[..]);
                            let fhi = find(&mut |e| starts_with_or_less(e, x));
                            assert_eq!((flo, fhi), (lo, hi), "k={}", fc.k);
                        }
                        c.search_fc[ki] = t.elapsed().as_nanos() as u64;
                    }
                    let mut g = total.lock().unwrap();
                    g.entries += c.entries;
                    g.hits += c.hits;
                    g.mismatch += c.mismatch;
                    g.scan_payload += c.scan_payload;
                    g.scan_entry += c.scan_entry;
                    g.fp_raw += c.fp_raw;
                    g.renders += c.renders;
                    g.render_raw += c.render_raw;
                    g.searches += c.searches;
                    g.search_raw += c.search_raw;
                    for i in 0..4 {
                        g.scan_fc[i] += c.scan_fc[i];
                        g.scan_fc_smart[i] += c.scan_fc_smart[i];
                        g.decode_fc[i] += c.decode_fc[i];
                        g.fp_fc[i] += c.fp_fc[i];
                        g.render_fc[i] += c.render_fc[i];
                        g.search_fc[i] += c.search_fc[i];
                    }
                }
            });
        }
    });
    let g = total.lock().unwrap().clone();
    let per = |ns: u64| ns as f64 / g.entries as f64;
    let ms = |ns: u64| ns as f64 / 1e6 / w as f64;
    println!("cpu attno {attno} needle {:?}: {} entries, {} hits, {} LIKE/NOT LIKE mismatches (exact form vs the raw scan, all k), wall {:.1} s, W = {w}; ns per entry (per-thread ms over the column)", String::from_utf8_lossy(needle), g.entries, g.hits, g.mismatch, t0.elapsed().as_secs_f64());
    println!("  LIKE scan today: payload scan {:.2} ns ({:.0} ms), per entry {:.2} ns ({:.0} ms)", per(g.scan_payload), ms(g.scan_payload), per(g.scan_entry), ms(g.scan_entry));
    println!("  fingerprint today: {:.2} ns ({:.0} ms)", per(g.fp_raw), ms(g.fp_raw));
    println!("  render by code today: {:.1} ns per render ({} renders per part)", g.render_raw as f64 / g.renders as f64, 100_000);
    println!("  prefix search today (both bounds): {:.0} ns per search", g.search_raw as f64 / g.searches as f64);
    for (ki, &k) in KS.iter().enumerate() {
        println!("  k={k}: decode alone {:.2} ns ({:.0} ms); LIKE decode+scan {:.2} ns ({:.0} ms), exact suffix-window form {:.2} ns ({:.0} ms); fingerprint {:.2} ns ({:.0} ms); render {:.1} ns; prefix search {:.0} ns", per(g.decode_fc[ki]), ms(g.decode_fc[ki]), per(g.scan_fc[ki]), ms(g.scan_fc[ki]), per(g.scan_fc_smart[ki]), ms(g.scan_fc_smart[ki]), per(g.fp_fc[ki]), ms(g.fp_fc[ki]), g.render_fc[ki] as f64 / g.renders as f64, g.search_fc[ki] as f64 / g.searches as f64);
    }
}

#[derive(Default, Clone, Copy)]
struct Expand {
    parts: u64,
    entries: u64,
    raw_bytes: u64,
    disk_raw: u64,
    disk_fc: [u64; 4],
    unzstd_raw: u64,
    unzstd_fc: [u64; 4],
    expand_fc: [u64; 4],
    stream_fc: [u64; 4],
    identical: u64,
}

fn frame_blocks(img: &[u8], n: usize, start: impl Fn(usize) -> usize) -> Vec<Vec<u8>> {
    (0..n.div_ceil(FRAME)).map(|f| zstd::bulk::compress(&img[start(f * FRAME)..start((f + 1) * FRAME)], 3).expect("zstd")).collect()
}

fn unzstd(blocks: &[Vec<u8>], out: &mut [u8]) -> usize {
    let mut at = 0usize;
    for b in blocks {
        at += zstd::bulk::decompress_to_buffer(b, &mut out[at..]).expect("unzstd");
    }
    at
}

fn expand(parts: &[Arc<OpenPart>], attno: u32, w: usize) {
    let binding = pgrc_am::scan::codec_binding();
    let total = Mutex::new(Expand::default());
    let next = AtomicUsize::new(0);
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..w {
            s.spawn(|| loop {
                let pi = next.fetch_add(1, Ordering::Relaxed);
                let Some(part) = parts.get(pi) else { break };
                let dir = part.stream_directory().expect("dir");
                if dir.lookup(attno, 0, StreamRole::Values).expect("values").entry.encoding != EncodingId::DictCodes as u16 {
                    continue;
                }
                let d = DictHandle::open(Arc::clone(part), None, binding.unwrappers, attno, 0).expect("dict");
                d.prewarm_payload().expect("prewarm");
                let payload = d.payload_bytes().expect("payload").expect("whole arm");
                let sp = Spans { index: d.index_bytes().expect("index") };
                let n = sp.n() as usize;
                let mut e = Expand { parts: 1, entries: n as u64, raw_bytes: payload.len() as u64, ..Expand::default() };
                let raw_blocks = frame_blocks(payload, n, |c| if c >= n { payload.len() } else { sp.span(c as u32).0 - 4 });
                e.disk_raw = raw_blocks.iter().map(|b| b.len() as u64).sum();
                let mut out = vec![0u8; payload.len()];
                let t = Instant::now();
                let got = unzstd(&raw_blocks, &mut out);
                e.unzstd_raw = t.elapsed().as_nanos() as u64;
                assert_eq!(got, payload.len());
                assert!(out == payload);
                for (ki, &k) in KS.iter().enumerate() {
                    let fc = Fc::build(payload, &sp, k);
                    let fc_blocks = frame_blocks(&fc.img, n, |c| if c >= n { fc.img.len() } else { fc.off[c] as usize });
                    e.disk_fc[ki] = fc_blocks.iter().map(|b| b.len() as u64).sum();
                    let mut fcbuf = vec![0u8; fc.img.len()];
                    out.fill(0);
                    let t = Instant::now();
                    let got = unzstd(&fc_blocks, &mut fcbuf);
                    e.unzstd_fc[ki] = t.elapsed().as_nanos() as u64;
                    assert_eq!(got, fc.img.len());
                    let t = Instant::now();
                    let (mut pos, mut prev) = (0usize, 0usize);
                    for c in 0..n {
                        let (off, l) = sp.span(c as u32);
                        out[off - 4..off].copy_from_slice(&(((l + 4) as u32) << 2).to_le_bytes());
                        if c % k == 0 {
                            pos += 4;
                            out[off..off + l].copy_from_slice(&fcbuf[pos..pos + l]);
                            pos += l;
                        } else {
                            let p = get_varint(&fcbuf, &mut pos);
                            out.copy_within(prev..prev + p, off);
                            out[off + p..off + l].copy_from_slice(&fcbuf[pos..pos + l - p]);
                            pos += l - p;
                        }
                        prev = off;
                    }
                    e.expand_fc[ki] = t.elapsed().as_nanos() as u64;
                    e.identical += (out == payload) as u64;
                    let mut out2 = vec![0u8; payload.len()];
                    let t = Instant::now();
                    let (mut c, mut prev) = (0usize, 0usize);
                    for b in &fc_blocks {
                        let mut dec = std::io::BufReader::with_capacity(1 << 16, zstd::stream::read::Decoder::new(&b[..]).expect("decoder"));
                        let end = (c + FRAME).min(n);
                        while c < end {
                            let (off, l) = sp.span(c as u32);
                            out2[off - 4..off].copy_from_slice(&(((l + 4) as u32) << 2).to_le_bytes());
                            if c % k == 0 {
                                let mut hdr = [0u8; 4];
                                dec.read_exact(&mut hdr).expect("hdr");
                                dec.read_exact(&mut out2[off..off + l]).expect("entry");
                            } else {
                                let (mut p, mut shift) = (0usize, 0);
                                loop {
                                    let mut byte = [0u8; 1];
                                    dec.read_exact(&mut byte).expect("varint");
                                    p |= ((byte[0] & 0x7f) as usize) << shift;
                                    if byte[0] < 0x80 {
                                        break;
                                    }
                                    shift += 7;
                                }
                                out2.copy_within(prev..prev + p, off);
                                dec.read_exact(&mut out2[off + p..off + l]).expect("suffix");
                            }
                            prev = off;
                            c += 1;
                        }
                    }
                    e.stream_fc[ki] = t.elapsed().as_nanos() as u64;
                    assert!(out2 == out, "streaming expand of part {pi} at k={k}");
                    for c in 0..n as u32 {
                        let (off, l) = sp.span(c);
                        assert!(out[off - 4..off + l] == payload[off - 4..off + l], "entry {c} of part {pi} at k={k}");
                    }
                }
                let mut g = total.lock().unwrap();
                g.parts += 1;
                g.entries += e.entries;
                g.raw_bytes += e.raw_bytes;
                g.disk_raw += e.disk_raw;
                g.unzstd_raw += e.unzstd_raw;
                g.identical += e.identical;
                for i in 0..4 {
                    g.disk_fc[i] += e.disk_fc[i];
                    g.unzstd_fc[i] += e.unzstd_fc[i];
                    g.expand_fc[i] += e.expand_fc[i];
                    g.stream_fc[i] += e.stream_fc[i];
                }
            });
        }
    });
    let g = total.lock().unwrap().clone();
    let per_part = |ns: u64| ns as f64 / 1e6 / g.parts as f64;
    let gbs = |ns: u64| g.raw_bytes as f64 / ns as f64;
    println!("expand attno {attno}: {} dict parts, {} entries, raw image {:.1} MB ({:.2} MB per part), W = {w}, wall {:.1} s; {} of {} expansions byte-identical to the raw image (entry images all equal by assertion)", g.parts, g.entries, g.raw_bytes as f64 / 1e6, g.raw_bytes as f64 / 1e6 / g.parts as f64, t0.elapsed().as_secs_f64(), g.identical, g.parts * 4);
    println!("  today: zstd-3 per frame {:.1} MB on disk; unwrap {:.3} ms per part ({:.2} GB/s of raw image)", g.disk_raw as f64 / 1e6, per_part(g.unzstd_raw), gbs(g.unzstd_raw));
    for (ki, &k) in KS.iter().enumerate() {
        let tot = g.unzstd_fc[ki] + g.expand_fc[ki];
        println!("  k={k}: {:.1} MB on disk ({:.3} of today); two-pass: unwrap {:.3} ms per part + expand {:.3} ms per part ({:.1} ns per entry) = {:.3} ms per part ({:.2} GB/s of raw image), {:.3} of today's unwrap; one-pass streaming inflate into the expander (64 KiB window) {:.3} ms per part ({:.1} ns per entry, {:.2} GB/s)", g.disk_fc[ki] as f64 / 1e6, g.disk_fc[ki] as f64 / g.disk_raw as f64, per_part(g.unzstd_fc[ki]), per_part(g.expand_fc[ki]), g.expand_fc[ki] as f64 / g.entries as f64, per_part(tot), gbs(tot), tot as f64 / g.unzstd_raw as f64, per_part(g.stream_fc[ki]), g.stream_fc[ki] as f64 / g.entries as f64, gbs(g.stream_fc[ki]));
    }
}

const ANCHOR: usize = 64;

fn bits(v: u32) -> u32 {
    32 - v.leading_zeros()
}

fn pack(vals: &[u32], width: u32) -> Vec<u8> {
    let mut out = vec![0u8; ((vals.len() * width as usize).div_ceil(64) + 1) * 8];
    for (i, &v) in vals.iter().enumerate() {
        let bit = i * width as usize;
        let at = bit / 8;
        let cur = u64::from_le_bytes(out[at..at + 8].try_into().unwrap()) | (v as u64) << (bit % 8);
        out[at..at + 8].copy_from_slice(&cur.to_le_bytes());
    }
    out
}

#[inline(always)]
fn unpack(packed: &[u8], i: usize, width: u32) -> u32 {
    let bit = i * width as usize;
    let at = bit / 8;
    (u64::from_le_bytes(packed[at..at + 8].try_into().unwrap()) >> (bit % 8)) as u32 & ((1u64 << width) - 1) as u32
}

fn utf8_chars(b: &[u8]) -> u32 {
    b.iter().filter(|&&c| c & 0xC0 != 0x80).count() as u32
}

#[derive(Default, Clone, Copy)]
struct IndexStats {
    parts: u64,
    entries: u64,
    index_disk: u64,
    zstd_index: u64,
    derivable_violations: u64,
    stored_char_wrong: u64,
    non_ascii_parts: u64,
    len_bits_part: u64,
    len_bits_frame: u64,
    delta_bits_part: u64,
    delta_bits_frame: u64,
    packed_part: u64,
    packed_frame: u64,
    zstd_packed_frame: u64,
    expand_ns: u64,
    pad_bytes: u64,
}

fn index(parts: &[Arc<OpenPart>], attnos: &[u32], w: usize) {
    let binding = pgrc_am::scan::codec_binding();
    let stats: Vec<Mutex<IndexStats>> = attnos.iter().map(|_| Mutex::new(IndexStats::default())).collect();
    let next = AtomicUsize::new(0);
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..w {
            s.spawn(|| loop {
                let pi = next.fetch_add(1, Ordering::Relaxed);
                let Some(part) = parts.get(pi) else { break };
                let dir = part.stream_directory().expect("dir");
                for (ai, &attno) in attnos.iter().enumerate() {
                    if dir.lookup(attno, 0, StreamRole::Values).expect("values").entry.encoding != EncodingId::DictCodes as u16 {
                        continue;
                    }
                    let d = DictHandle::open(Arc::clone(part), None, binding.unwrappers, attno, 0).expect("dict");
                    d.prewarm_payload().expect("prewarm");
                    let payload = d.payload_bytes().expect("payload").expect("whole arm");
                    let index = d.index_bytes().expect("index");
                    let sp = Spans { index };
                    let n = sp.n() as usize;
                    let mut st = IndexStats { parts: 1, entries: n as u64, ..IndexStats::default() };
                    st.index_disk = dir.lookup(attno, 0, StreamRole::DictIndex).expect("index stream").extents.iter().map(|e| e.len).sum();
                    st.zstd_index = zstd::bulk::compress(index, 3).expect("zstd").len() as u64;
                    let (mut lens, mut deltas, mut anchors) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n / ANCHOR + 1));
                    let mut expect = 4usize;
                    for c in 0..n {
                        let (off, l) = sp.span(c as u32);
                        if c % ANCHOR == 0 {
                            anchors.push((off - 4) as u32);
                        }
                        if off != expect {
                            st.derivable_violations += 1;
                            expect = off;
                        }
                        st.pad_bytes += ((4 + l).next_multiple_of(8) - l) as u64;
                        expect += (4 + l).next_multiple_of(8);
                        let e = &payload[off..off + l];
                        let chars = utf8_chars(e);
                        let stored = u32::from_le_bytes(index[c * 12 + 8..c * 12 + 12].try_into().unwrap());
                        st.stored_char_wrong += (stored != chars) as u64;
                        lens.push(l as u32);
                        deltas.push(l as u32 - chars);
                    }
                    let wl = bits(*lens.iter().max().unwrap_or(&0));
                    let wd = bits(*deltas.iter().max().unwrap_or(&0));
                    st.non_ascii_parts = (wd > 0) as u64;
                    st.len_bits_part = wl as u64 * n as u64;
                    st.delta_bits_part = wd as u64 * n as u64;
                    let frames = n.div_ceil(FRAME);
                    let (mut fl, mut fd) = (Vec::with_capacity(frames), Vec::with_capacity(frames));
                    for f in 0..frames {
                        let r = f * FRAME..((f + 1) * FRAME).min(n);
                        let (lo, hi) = (*lens[r.clone()].iter().min().unwrap(), *lens[r.clone()].iter().max().unwrap());
                        fl.push((lo, bits(hi - lo)));
                        fd.push(bits(*deltas[r.clone()].iter().max().unwrap()));
                        st.len_bits_frame += bits(hi - lo) as u64 * r.len() as u64;
                        st.delta_bits_frame += fd[f] as u64 * r.len() as u64;
                    }
                    st.packed_part = anchors.len() as u64 * 4 + (st.len_bits_part + st.delta_bits_part).div_ceil(8) + 2;
                    st.packed_frame = anchors.len() as u64 * 4 + (st.len_bits_frame + st.delta_bits_frame).div_ceil(8) + frames as u64 * 8;
                    let packed_lens = pack(&lens, wl);
                    let packed_deltas = pack(&deltas, wd);
                    let mut fimg = Vec::new();
                    for f in 0..frames {
                        let r = f * FRAME..((f + 1) * FRAME).min(n);
                        let rel: Vec<u32> = lens[r.clone()].iter().map(|&l| l - fl[f].0).collect();
                        fimg.extend_from_slice(&pack(&rel, fl[f].1)[..(rel.len() * fl[f].1 as usize).div_ceil(8)]);
                        fimg.extend_from_slice(&pack(&deltas[r.clone()], fd[f])[..(r.len() * fd[f] as usize).div_ceil(8)]);
                    }
                    for a in &anchors {
                        fimg.extend_from_slice(&a.to_le_bytes());
                    }
                    st.zstd_packed_frame = zstd::bulk::compress(&fimg, 3).expect("zstd").len() as u64;
                    let mut out = vec![0u8; n * 12];
                    let t = Instant::now();
                    let mut off = 0u32;
                    for c in 0..n {
                        if c % ANCHOR == 0 {
                            off = anchors[c / ANCHOR];
                        }
                        let l = unpack(&packed_lens, c, wl);
                        let ch = l - unpack(&packed_deltas, c, wd);
                        out[c * 12..c * 12 + 4].copy_from_slice(&off.to_le_bytes());
                        out[c * 12 + 4..c * 12 + 8].copy_from_slice(&l.to_le_bytes());
                        out[c * 12 + 8..c * 12 + 12].copy_from_slice(&ch.to_le_bytes());
                        off += (4 + l).next_multiple_of(8);
                    }
                    st.expand_ns = t.elapsed().as_nanos() as u64;
                    for c in 0..n {
                        assert!(out[c * 12..c * 12 + 8] == index[c * 12..c * 12 + 8], "part {pi} attno {attno} entry {c}: offset/len");
                    }
                    let mut g = stats[ai].lock().unwrap();
                    g.parts += 1;
                    g.entries += st.entries;
                    g.index_disk += st.index_disk;
                    g.zstd_index += st.zstd_index;
                    g.derivable_violations += st.derivable_violations;
                    g.stored_char_wrong += st.stored_char_wrong;
                    g.non_ascii_parts += st.non_ascii_parts;
                    g.len_bits_part += st.len_bits_part;
                    g.len_bits_frame += st.len_bits_frame;
                    g.delta_bits_part += st.delta_bits_part;
                    g.delta_bits_frame += st.delta_bits_frame;
                    g.packed_part += st.packed_part;
                    g.packed_frame += st.packed_frame;
                    g.zstd_packed_frame += st.zstd_packed_frame;
                    g.expand_ns += st.expand_ns;
                    g.pad_bytes += st.pad_bytes;
                }
            });
        }
    });
    eprintln!("index: wall {:.1} s, W = {w}", t0.elapsed().as_secs_f64());
    let mb = |b: u64| b as f64 / 1e6;
    for (ai, &attno) in attnos.iter().enumerate() {
        let s = stats[ai].lock().unwrap().clone();
        let n = s.entries as f64;
        println!("attno {attno}: {} dict parts, {} entries; index today {:.1} MB raw (12 B/entry), {:.1} MB on disk (zstd-3 whole: {:.1} MB); payload_off derivable from the 8-aligned entry sizes: {} violations; stored char field != UTF-8 count: {} entries; parts with a non-ASCII entry: {}; payload header+padding {:.1} MB ({:.2} B/entry)", s.parts, s.entries, mb(s.entries * 12), mb(s.index_disk), mb(s.zstd_index), s.derivable_violations, s.stored_char_wrong, s.non_ascii_parts, mb(s.pad_bytes), s.pad_bytes as f64 / n);
        println!("  packed, part FOR: byte_len {:.2} bits/entry, char delta {:.2} bits/entry, anchors every {ANCHOR}: {:.1} MB ({:.2} B/entry, {:.3} of raw)", s.len_bits_part as f64 / n, s.delta_bits_part as f64 / n, mb(s.packed_part), s.packed_part as f64 / n, s.packed_part as f64 / (12.0 * n));
        println!("  packed, frame FOR: byte_len {:.2} bits/entry, char delta {:.2} bits/entry: {:.1} MB ({:.2} B/entry, {:.3} of raw); zstd-3 of it {:.1} MB ({:.3} of today's on-disk index)", s.len_bits_frame as f64 / n, s.delta_bits_frame as f64 / n, mb(s.packed_frame), s.packed_frame as f64 / n, s.packed_frame as f64 / (12.0 * n), mb(s.zstd_packed_frame), s.zstd_packed_frame as f64 / s.index_disk as f64);
        println!("  expand to 12 B records (prefix sum + unpack, part FOR): {:.2} ns/entry, {:.3} ms per part", s.expand_ns as f64 / n, s.expand_ns as f64 / 1e6 / s.parts as f64);
    }
}

#[derive(Default, Clone)]
struct Resident {
    parts: u64,
    entries: u64,
    payload: u64,
    index: u64,
    disk_payload: u64,
    fault_ns: u64,
    random_first_ns: u64,
    random_hit_ns: u64,
    random_n: u64,
    walk_ns: u64,
    like_ns: u64,
    like_hits: u64,
    materialized: u64,
}

/// The resident form the process's arm serves (`PGRUST_PGRC_DICT_RESIDENT`), measured through the
/// handle: every dictionary part of each column faulted whole from a warm page cache (the fault
/// path), then `RANDOM` random codes resolved twice (first touch, then the memo / the image), a full
/// cursor walk, and a `LIKE '%garden%'` table build the way `text_hosts` builds it.
fn resident(parts: &[Arc<OpenPart>], attnos: &[u32], w: usize) {
    const RANDOM: usize = 20_000;
    let binding = pgrc_am::scan::codec_binding();
    let stats: Vec<Mutex<Resident>> = attnos.iter().map(|_| Mutex::new(Resident::default())).collect();
    let next = AtomicUsize::new(0);
    let arm = if pgrc_read::dict_resident_frontcoded() { "frontcoded" } else { "expanded" };
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..w {
            s.spawn(|| loop {
                let pi = next.fetch_add(1, Ordering::Relaxed);
                let Some(part) = parts.get(pi) else { break };
                let dir = part.stream_directory().expect("dir");
                for (ai, &attno) in attnos.iter().enumerate() {
                    if dir.lookup(attno, 0, StreamRole::Values).expect("values").entry.encoding != EncodingId::DictCodes as u16 {
                        continue;
                    }
                    let mut r = Resident { parts: 1, ..Resident::default() };
                    r.disk_payload = dir.lookup(attno, 0, StreamRole::DictPayload).expect("payload stream").extents.iter().map(|e| e.len).sum();
                    let t = Instant::now();
                    let d = DictHandle::open(Arc::clone(part), None, binding.unwrappers, attno, 0).expect("dict");
                    d.prewarm_payload().expect("prewarm");
                    let index = d.index_bytes().expect("index");
                    r.fault_ns = t.elapsed().as_nanos() as u64;
                    let n = d.ncodes();
                    r.entries = n as u64;
                    r.payload = d.resident_payload_bytes();
                    r.index = index.len() as u64;
                    let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ (pi as u64) << 32 ^ attno as u64;
                    let mut codes = Vec::with_capacity(RANDOM);
                    for _ in 0..RANDOM {
                        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                        codes.push((x % n.max(1) as u64) as u32);
                    }
                    let mut sink = 0u64;
                    let t = Instant::now();
                    for &c in &codes {
                        sink = sink.wrapping_add(d.entry(c).expect("entry").bytes.len() as u64);
                    }
                    r.random_first_ns = t.elapsed().as_nanos() as u64;
                    let t = Instant::now();
                    for &c in &codes {
                        sink = sink.wrapping_add(d.entry(c).expect("entry").bytes.len() as u64);
                    }
                    r.random_hit_ns = t.elapsed().as_nanos() as u64;
                    r.random_n = RANDOM as u64;
                    r.materialized = d.fc_materialized_bytes();
                    let t = Instant::now();
                    let mut cur = d.entries(0, n).expect("cursor");
                    while let Some((_, e)) = cur.next_entry().expect("next") {
                        sink = sink.wrapping_add(e.bytes.len() as u64);
                    }
                    drop(cur);
                    r.walk_ns = t.elapsed().as_nanos() as u64;
                    let t = Instant::now();
                    let err = std::cell::Cell::new(None);
                    let view = gravity_hosts::dict::text_hosts::DictView::Part(&d, &err);
                    let spec = gravity_hosts::dict::text_hosts::TableSpec { over: 0, func: gravity_hosts::dict::text_hosts::TableFn::Like { pattern: b"%garden%".to_vec(), ilike: false, negated: false } };
                    let tbl = gravity_hosts::dict::text_hosts::build_owned(n as usize, 0, &mut |region| gravity_hosts::dict::text_hosts::build_table(&spec, &view, region, &mut None)).expect("like table");
                    r.like_ns = t.elapsed().as_nanos() as u64;
                    let bits = unsafe { gravity_dict::Bits::at(tbl.header()) };
                    r.like_hits = (0..n).filter(|&c| unsafe { bits.get(c) }).count() as u64;
                    std::hint::black_box(sink);
                    let mut g = stats[ai].lock().unwrap();
                    g.parts += r.parts; g.entries += r.entries; g.payload += r.payload; g.index += r.index; g.disk_payload += r.disk_payload;
                    g.fault_ns += r.fault_ns; g.random_first_ns += r.random_first_ns; g.random_hit_ns += r.random_hit_ns; g.random_n += r.random_n;
                    g.walk_ns += r.walk_ns; g.like_ns += r.like_ns; g.like_hits += r.like_hits; g.materialized += r.materialized;
                }
            });
        }
    });
    eprintln!("resident ({arm}): {} parts, {} columns, W={w}, wall {:.1} s", parts.len(), attnos.len(), t0.elapsed().as_secs_f64());
    let mb = |b: u64| b as f64 / 1e6;
    for (ai, &attno) in attnos.iter().enumerate() {
        let s = stats[ai].lock().unwrap().clone();
        if s.parts == 0 { continue; }
        let per_part = |ns: u64| ns as f64 / 1e6 / s.parts as f64;
        println!("attno {attno} [{arm}]: {} dict parts, {} entries; resident payload {:.1} MB (on disk {:.1} MB), index {:.1} MB; fault path {:.2} ms/part; random entry first {:.0} ns, again {:.0} ns (materialized {:.1} MB over {} random codes/part); walk {:.1} ns/entry ({:.2} ms/part); LIKE '%garden%' table {:.2} ms/part ({} hits)",
            s.parts, s.entries, mb(s.payload), mb(s.disk_payload), mb(s.index), per_part(s.fault_ns), s.random_first_ns as f64 / s.random_n.max(1) as f64, s.random_hit_ns as f64 / s.random_n.max(1) as f64, mb(s.materialized), RANDOM, s.walk_ns as f64 / s.entries.max(1) as f64, per_part(s.walk_ns), per_part(s.like_ns), s.like_hits);
    }
}
