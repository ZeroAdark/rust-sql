//! Scratch bank under the pgrc format 2.2 dictionary wrappers, built from a sealed bank by rewriting ONLY the
//! dictionary streams of every part (lane fcwriter; docs/gravity/frontcode-measurement.md, "as landed"):
//! every other section is relocated byte-identically; the manifest, CURRENT and the bankstats identity
//! vector follow the moved footers. Never run against a template directory as `dst`.
//!
//!   fcbank reseal <src table dir> <dst table dir> [w]   rewrite each part (FrontZstd payload, PackedZstd
//!                                                       index), copy the sidecars; on-disk bytes per column
//!   fcbank diff <src table dir> <dst table dir> [w]     expand every dictionary of every part pair through
//!                                                       the reader, compare index/payload entry by entry; the
//!                                                       fault path (prewarm) per part, both arms (use w = 1)

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use pgrc_format::dict::{utf8_char_count, DictCharLenForm, DICT_INDEX_ENTRY_LEN};
use pgrc_format::enc::{EncodingId, Wrapper};
use pgrc_format::manifest::{CommitPointer, Manifest};
use pgrc_format::part::{
    ExtentRecord, FooterFixed, PartTail, SectionEntry, SectionKind, StreamEntry, StreamRole,
    EXTENT_RECORD_LEN, PART_HEADER_LEN, SECTION_ENTRY_LEN, STREAM_ENTRY_LEN, STREAM_SECTION_HDR_LEN,
};
use pgrc_format::wire::{crc32c, Cur};
use pgrc_read::{DictHandle, MemPartIo, OpenPart, PartExpect};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let w: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(8);
    match a.get(1).map(String::as_str) {
        Some("reseal") => reseal(&a[2], &a[3], w),
        Some("diff") => diff(&a[2], &a[3], w),
        _ => eprintln!("usage: fcbank reseal|diff <src table dir> <dst table dir> [w]"),
    }
}

fn part_files(dir: &str) -> Vec<(u32, std::path::PathBuf)> {
    let mut v: Vec<(u32, std::path::PathBuf)> = std::fs::read_dir(dir)
        .expect("table dir")
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            let n = p.file_name()?.to_string_lossy().into_owned();
            pgrc_format::dirlayout::parse_part_file_name(&n).map(|no| (no, p))
        })
        .collect();
    v.sort();
    v
}

fn pad8(v: &mut Vec<u8>) {
    while v.len() % 8 != 0 {
        v.push(0);
    }
}

#[derive(Default, Clone)]
struct ColBytes {
    parts: u64,
    entries: u64,
    index_before: u64,
    index_after: u64,
    index_utf8: u64,
    payload_before: u64,
    payload_after: u64,
    kept: u64,
}

struct Rewritten {
    bytes: Vec<u8>,
    footer_off: u64,
    census: Vec<(u32, ColBytes)>,
}

/// The unwrapped image of a byte-run stream's one section (v1 index/payload sections).
fn unwrapped_section(bytes: &[u8], sec: &SectionEntry, entry: &StreamEntry) -> Vec<u8> {
    let raw = &bytes[sec.off as usize..][..sec.len as usize];
    match Wrapper::from_u8(entry.wrapper).expect("wrapper") {
        Wrapper::None => raw.to_vec(),
        Wrapper::Lz4 | Wrapper::Zstd | Wrapper::OnPairZstd | Wrapper::IntPre => {
            let mut out = Vec::new();
            pgrc_codec::wrapper::unwrap_section(raw, &mut out).expect("unwrap");
            out
        }
        Wrapper::FrontZstd | Wrapper::PackedZstd | Wrapper::FrontOnPairZstd => panic!("source bank already carries the pgrc format 2.2 layers"),
    }
}

fn rewrite_part(bytes: &[u8]) -> Rewritten {
    let tail = PartTail::decode_at_eof(bytes).expect("tail");
    let footer = FooterFixed::decode(&bytes[tail.footer_off as usize..]).expect("footer");
    let st = footer.section_table_off as usize;
    let sections: Vec<SectionEntry> = (0..footer.section_count as usize)
        .map(|i| SectionEntry::decode(&mut Cur::new(&bytes[st + i * SECTION_ENTRY_LEN..st + (i + 1) * SECTION_ENTRY_LEN])).expect("section"))
        .collect();
    let sd = sections.iter().position(|s| s.kind == SectionKind::StreamDir.as_u16()).expect("stream dir");
    let sdb = &bytes[sections[sd].off as usize..][..sections[sd].len as usize];
    let mut streams: Vec<(StreamEntry, Vec<ExtentRecord>)> = Vec::new();
    for i in 0..footer.stream_count as usize {
        let e = StreamEntry::decode(&mut Cur::new(&sdb[i * STREAM_ENTRY_LEN..(i + 1) * STREAM_ENTRY_LEN])).expect("entry");
        let t = e.extent_table_off as usize;
        let ex: Vec<ExtentRecord> = (0..e.extent_count as usize)
            .map(|k| ExtentRecord::decode(&mut Cur::new(&sdb[t + k * EXTENT_RECORD_LEN..t + (k + 1) * EXTENT_RECORD_LEN])).expect("extent"))
            .collect();
        streams.push((e, ex));
    }
    // Dictionary streams: (attno, path_ord) -> (index stream ix, payload stream ix), their sections.
    let sec_at = |off: u64| sections.iter().position(|s| s.kind == SectionKind::Stream.as_u16() && s.off == off).expect("dict section");
    let mut dicts: BTreeMap<(u32, u32), (usize, usize)> = BTreeMap::new();
    for (i, (e, _)) in streams.iter().enumerate() {
        match StreamRole::from_u8(e.role).expect("role") {
            StreamRole::DictIndex => dicts.entry((e.attno, e.path_ord)).or_insert((usize::MAX, usize::MAX)).0 = i,
            StreamRole::DictPayload => dicts.entry((e.attno, e.path_ord)).or_insert((usize::MAX, usize::MAX)).1 = i,
            _ => {}
        }
    }
    // Rebuilt section bytes keyed by section index, plus the stream's new wrapper byte.
    let mut rebuilt: BTreeMap<usize, (Vec<u8>, usize, u8)> = BTreeMap::new();
    let mut census: Vec<(u32, ColBytes)> = Vec::new();
    for (&(attno, _po), &(ii, pi)) in &dicts {
        let (ie, iex) = &streams[ii];
        let (pe, pex) = &streams[pi];
        let isec = sec_at(iex[0].file_off);
        let psec = sec_at(pex[0].file_off);
        let index = unwrapped_section(bytes, &sections[isec], ie);
        let payload = unwrapped_section(bytes, &sections[psec], pe);
        let index_recs = &index[STREAM_SECTION_HDR_LEN..];
        let mut cb = ColBytes { parts: 1, entries: ie.values, ..ColBytes::default() };
        cb.index_before = iex.iter().map(|x| x.len).sum();
        cb.payload_before = pex.iter().map(|x| x.len).sum();
        // Index: packed beneath zstd (Absolute char form only), under the writer's ≥20% law.
        let form = DictCharLenForm::from_flags(ie.flags).expect("form");
        let mut new_index = Vec::new();
        let index_wrapper = if form == DictCharLenForm::Absolute {
            pgrc_codec::wrapper::wrap_dict_index_section(&index, &mut new_index).expect("pack index");
            if pgrc_codec::wins_by_twenty_percent(new_index.len(), index.len()) { Wrapper::PackedZstd } else { Wrapper::None }
        } else {
            Wrapper::None
        };
        if index_wrapper == Wrapper::None {
            new_index = index.clone();
            cb.kept += 1;
        }
        // The same index with the REAL UTF-8 char count (what the new writer seals): the re-bank's bytes.
        {
            let ppay = &payload[STREAM_SECTION_HDR_LEN..];
            let mut recs = index_recs.to_vec();
            for c in 0..(recs.len() / DICT_INDEX_ENTRY_LEN) {
                let off = u32::from_le_bytes(recs[c * 12..c * 12 + 4].try_into().unwrap()) as usize;
                let len = u32::from_le_bytes(recs[c * 12 + 4..c * 12 + 8].try_into().unwrap()) as usize;
                let ch = utf8_char_count(&ppay[off + 4..off + 4 + len]);
                recs[c * 12 + 8..c * 12 + 12].copy_from_slice(&ch.to_le_bytes());
            }
            let mut packed = Vec::new();
            pgrc_format::dict::pack_dict_index(&recs, &mut packed).expect("pack");
            let z = zstd::bulk::compress(&packed, 3).expect("zstd");
            cb.index_utf8 = (STREAM_SECTION_HDR_LEN + 8 + z.len()) as u64;
        }
        let mut new_payload = Vec::new();
        pgrc_codec::wrapper::wrap_dict_payload_section(&payload, index_recs, &mut new_payload).expect("front code");
        let payload_wrapper = if pgrc_codec::wins_by_twenty_percent(new_payload.len(), payload.len()) {
            Wrapper::FrontZstd
        } else {
            new_payload = payload.clone();
            cb.kept += 1;
            Wrapper::None
        };
        cb.index_after = new_index.len() as u64;
        cb.payload_after = new_payload.len() as u64;
        census.push((attno, cb));
        rebuilt.insert(isec, (new_index, ii, index_wrapper.as_u8()));
        rebuilt.insert(psec, (new_payload, pi, payload_wrapper.as_u8()));
    }
    // Assembly in file order: header, sections (dict ones rebuilt, the rest byte-identical), the
    // stream directory re-encoded over the new extents, section table, footer, tail.
    let mut order: Vec<usize> = (0..sections.len()).collect();
    order.sort_by_key(|&i| sections[i].off);
    let mut out: Vec<u8> = bytes[..PART_HEADER_LEN].to_vec();
    let mut new_secs = sections.clone();
    let mut new_streams = streams.clone();
    let mut seen_dir = false;
    for &i in &order {
        pad8(&mut out);
        let new_off = out.len() as u64;
        let s = &sections[i];
        if let Some((img, si, wrapper)) = rebuilt.get(&i) {
            assert!(!seen_dir);
            out.extend_from_slice(img);
            new_secs[i] = SectionEntry { off: new_off, len: img.len() as u64, crc: crc32c(img), ..*s };
            let (e, ex) = &mut new_streams[*si];
            e.wrapper = *wrapper;
            e.extent_count = 1;
            *ex = vec![ExtentRecord {
                file_off: new_off,
                len: img.len() as u64,
                values: e.values,
                granule_start: 0,
                granule_count: footer.granule_count,
                crc: crc32c(img),
                flags: 0,
            }];
        } else if s.kind == SectionKind::StreamDir.as_u16() {
            seen_dir = true;
            let entries_len = new_streams.len() * STREAM_ENTRY_LEN;
            let mut tables: Vec<u8> = Vec::new();
            let mut body: Vec<u8> = Vec::new();
            for (e, ex) in new_streams.iter_mut() {
                e.extent_table_off = (entries_len + tables.len()) as u64;
                for x in ex.iter() {
                    x.encode_into(&mut tables);
                }
                e.encode_into(&mut body);
            }
            body.extend_from_slice(&tables);
            out.extend_from_slice(&body);
            new_secs[i] = SectionEntry { off: new_off, len: body.len() as u64, crc: crc32c(&body), ..*s };
        } else {
            assert!(!seen_dir || s.kind != SectionKind::Stream.as_u16());
            out.extend_from_slice(&bytes[s.off as usize..][..s.len as usize]);
            new_secs[i] = SectionEntry { off: new_off, ..*s };
            if s.kind == SectionKind::Stream.as_u16() {
                for (_, ex) in new_streams.iter_mut() {
                    for x in ex.iter_mut() {
                        if x.file_off >= s.off && x.file_off < s.off + s.len {
                            x.file_off = new_off + (x.file_off - s.off);
                        }
                    }
                }
            }
        }
    }
    pad8(&mut out);
    let section_table_off = out.len() as u64;
    let mut table = Vec::new();
    for s in &new_secs {
        s.encode_into(&mut table);
    }
    let section_table_crc = crc32c(&table);
    out.extend_from_slice(&table);
    pad8(&mut out);
    let footer_off = out.len() as u64;
    FooterFixed { section_table_off, section_table_crc, footer_crc: 0, ..footer }.encode_into(&mut out);
    PartTail::new(footer_off).encode_into(&mut out);
    Rewritten { bytes: out, footer_off, census }
}

fn reseal(src: &str, dst: &str, w: usize) {
    assert!(!dst.contains("template"), "never write into a template");
    std::fs::create_dir_all(dst).expect("dst");
    let parts = part_files(src);
    let next = AtomicUsize::new(0);
    let totals: Mutex<BTreeMap<u32, ColBytes>> = Mutex::new(BTreeMap::new());
    let footers: Mutex<BTreeMap<u32, (u64, u64)>> = Mutex::new(BTreeMap::new());
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..w {
            s.spawn(|| loop {
                let pi = next.fetch_add(1, Ordering::Relaxed);
                let Some((no, path)) = parts.get(pi) else { break };
                let bytes = std::fs::read(path).expect("part");
                let r = rewrite_part(&bytes);
                let out = std::path::Path::new(dst).join(pgrc_format::dirlayout::part_file_name(*no));
                std::fs::write(&out, &r.bytes).expect("write part");
                footers.lock().unwrap().insert(*no, (r.bytes.len() as u64, r.footer_off));
                let mut t = totals.lock().unwrap();
                for (attno, cb) in r.census {
                    let e = t.entry(attno).or_default();
                    e.parts += cb.parts;
                    e.entries += cb.entries;
                    e.index_before += cb.index_before;
                    e.index_after += cb.index_after;
                    e.index_utf8 += cb.index_utf8;
                    e.payload_before += cb.payload_before;
                    e.payload_after += cb.payload_after;
                    e.kept += cb.kept;
                }
            });
        }
    });
    // Sidecars byte-identical; manifest and CURRENT follow the moved footers; the bankstats plane's
    // identity vector (footer_off per part) is patched so the plane stays valid for the lab.
    let footers = footers.lock().unwrap().clone();
    for e in std::fs::read_dir(src).expect("src").filter_map(|e| e.ok()) {
        let n = e.file_name().to_string_lossy().into_owned();
        if pgrc_format::dirlayout::parse_part_file_name(&n).is_some() {
            continue;
        }
        let to = std::path::Path::new(dst).join(&n);
        if let Some(gen) = pgrc_format::dirlayout::parse_manifest_file_name(&n) {
            let mut m = Manifest::decode(&std::fs::read(e.path()).expect("manifest")).expect("manifest decode");
            for p in m.parts.iter_mut() {
                let (len, fo) = footers[&p.part_no];
                p.file_len = len;
                p.footer_off = fo;
            }
            let img = m.encode();
            std::fs::write(&to, &img).expect("manifest");
            let cur = std::fs::read(std::path::Path::new(src).join("CURRENT")).expect("CURRENT");
            let cp = CommitPointer::decode(&cur).expect("commit pointer");
            if cp.gen == gen {
                let cp = CommitPointer::new(gen, img.len() as u64, crc32c(&img[..img.len() - 4]));
                std::fs::write(std::path::Path::new(dst).join("CURRENT"), cp.encode()).expect("CURRENT");
            }
        } else if pgrc_format::bankstats::parse_bankstats_file_name(&n).is_some() {
            let mut b = std::fs::read(e.path()).expect("bankstats");
            let part_count = u32::from_le_bytes(b[24..28].try_into().unwrap()) as usize;
            let meta_len = u64::from_le_bytes(b[32..40].try_into().unwrap()) as usize;
            for k in 0..part_count {
                let row = 48 + k * 32;
                let no = u32::from_le_bytes(b[row..row + 4].try_into().unwrap());
                let (_, fo) = footers[&no];
                b[row + 24..row + 32].copy_from_slice(&fo.to_le_bytes());
            }
            let crc = crc32c(&b[..meta_len - 4]);
            b[meta_len - 4..meta_len].copy_from_slice(&crc.to_le_bytes());
            std::fs::write(&to, &b).expect("bankstats");
        } else if n != "CURRENT" {
            std::fs::copy(e.path(), &to).expect("copy");
        }
    }
    let mb = |b: u64| b as f64 / 1e6;
    println!("reseal: {} parts, W = {w}, wall {:.1} s -> {dst}", parts.len(), t0.elapsed().as_secs_f64());
    for (attno, c) in totals.lock().unwrap().iter() {
        println!(
            "attno {attno}: {} dict parts, {} entries; index on disk {:.1} -> {:.1} MB ({:.3}; with real UTF-8 char deltas {:.1} MB, {:.3}); payload on disk {:.1} -> {:.1} MB ({:.3}); {} sections kept unwrapped by the 20% law",
            c.parts, c.entries, mb(c.index_before), mb(c.index_after), c.index_after as f64 / c.index_before as f64, mb(c.index_utf8), c.index_utf8 as f64 / c.index_before as f64,
            mb(c.payload_before), mb(c.payload_after), c.payload_after as f64 / c.payload_before as f64, c.kept
        );
    }
}

#[derive(Default, Clone)]
struct DiffStats {
    parts: u64,
    entries: u64,
    mismatches: u64,
    image_mismatch: u64,
    src_ns: u64,
    dst_ns: u64,
    src_bytes: u64,
    dst_bytes: u64,
}

fn open_mem(path: &std::path::Path, ino: u64) -> std::sync::Arc<OpenPart> {
    let bytes = std::fs::read(path).expect("part");
    std::sync::Arc::new(OpenPart::open(Box::new(MemPartIo::new(bytes, 1, ino)), &PartExpect::none()).expect("open"))
}

fn diff(src: &str, dst: &str, w: usize) {
    let a = part_files(src);
    let b = part_files(dst);
    assert_eq!(a.len(), b.len(), "part counts differ");
    let binding = pgrc_am::scan::codec_binding();
    let next = AtomicUsize::new(0);
    let totals: Mutex<BTreeMap<u32, DiffStats>> = Mutex::new(BTreeMap::new());
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..w {
            s.spawn(|| loop {
                let pi = next.fetch_add(1, Ordering::Relaxed);
                let Some((no, pa)) = a.get(pi) else { break };
                let (nb, pb) = &b[pi];
                assert_eq!(no, nb);
                let (pa, pb) = (open_mem(pa, 2 * pi as u64), open_mem(pb, 2 * pi as u64 + 1));
                let da = pa.stream_directory().expect("dir");
                let db = pb.stream_directory().expect("dir");
                let attnos: Vec<u32> = da
                    .streams()
                    .iter()
                    .filter(|s| s.role == StreamRole::DictIndex && s.entry.path_ord == 0)
                    .map(|s| s.entry.attno)
                    .collect();
                for attno in attnos {
                    let va = da.lookup(attno, 0, StreamRole::Values).expect("values");
                    if va.entry.encoding != EncodingId::DictCodes.as_u16() {
                        continue;
                    }
                    let mut st = DiffStats { parts: 1, ..DiffStats::default() };
                    for role in [StreamRole::DictIndex, StreamRole::DictPayload] {
                        st.src_bytes += da.lookup(attno, 0, role).expect("s").extents.iter().map(|x| x.len).sum::<u64>();
                        st.dst_bytes += db.lookup(attno, 0, role).expect("s").extents.iter().map(|x| x.len).sum::<u64>();
                    }
                    let ha = DictHandle::open(pa.clone(), None, binding.unwrappers, attno, 0).expect("dict a");
                    let hb = DictHandle::open(pb.clone(), None, binding.unwrappers, attno, 0).expect("dict b");
                    let t = Instant::now();
                    ha.prewarm_payload().expect("prewarm a");
                    st.src_ns = t.elapsed().as_nanos() as u64;
                    let t = Instant::now();
                    hb.prewarm_payload().expect("prewarm b");
                    st.dst_ns = t.elapsed().as_nanos() as u64;
                    let n = ha.ncodes();
                    assert_eq!(n, hb.ncodes(), "part {no} attno {attno}: entry count");
                    st.entries = n as u64;
                    if ha.index_bytes().expect("ia") != hb.index_bytes().expect("ib") {
                        st.image_mismatch += 1;
                    }
                    let (pa_img, pb_img) = (ha.payload_bytes().expect("pa"), hb.payload_bytes().expect("pb"));
                    if pa_img != pb_img {
                        st.image_mismatch += 1;
                    }
                    for c in 0..n {
                        let ea = ha.entry(c).expect("entry a");
                        let eb = hb.entry(c).expect("entry b");
                        if ea.image != eb.image || ea.byte_len != eb.byte_len || ea.char_len != eb.char_len {
                            st.mismatches += 1;
                        }
                        let (la, lb) = (ha.lengths(c).expect("la"), hb.lengths(c).expect("lb"));
                        if la != lb {
                            st.mismatches += 1;
                        }
                    }
                    let mut t = totals.lock().unwrap();
                    let e = t.entry(attno).or_default();
                    e.parts += 1;
                    e.entries += st.entries;
                    e.mismatches += st.mismatches;
                    e.image_mismatch += st.image_mismatch;
                    e.src_ns += st.src_ns;
                    e.dst_ns += st.dst_ns;
                    e.src_bytes += st.src_bytes;
                    e.dst_bytes += st.dst_bytes;
                }
            });
        }
    });
    println!("diff: {} part pairs, W = {w}, wall {:.1} s", a.len(), t0.elapsed().as_secs_f64());
    let (mut te, mut tm) = (0u64, 0u64);
    for (attno, s) in totals.lock().unwrap().iter() {
        te += s.entries;
        tm += s.mismatches + s.image_mismatch;
        println!(
            "attno {attno}: {} dict parts, {} entries, {} entry mismatches, {} image mismatches; fault path (prewarm) per part {:.3} ms -> {:.3} ms ({:.3}); dict bytes on disk {:.1} -> {:.1} MB ({:.3})",
            s.parts, s.entries, s.mismatches, s.image_mismatch,
            s.src_ns as f64 / 1e6 / s.parts as f64, s.dst_ns as f64 / 1e6 / s.parts as f64, s.dst_ns as f64 / s.src_ns.max(1) as f64,
            s.src_bytes as f64 / 1e6, s.dst_bytes as f64 / 1e6, s.dst_bytes as f64 / s.src_bytes.max(1) as f64
        );
    }
    println!("TOTAL entries {te}, mismatches {tm}");
}
