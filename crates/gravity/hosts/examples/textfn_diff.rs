//! The text-table differential over a real pgrc table (hits): every dictionary part's LIKE,
//! length and regexp_replace tables through `text_hosts::build_table` (the production path)
//! against per-entry oracles — `adt_like` (like_match.c), the entry bytes, RE2 through the
//! fmgr's replacement routine — over every entry; and the contains-scan A/B Michael asked for
//! (the rare-byte-pair scan vs memchr's two-way `Finder`, per entry and over the payload).
//!
//!   textfn_diff <table dir> <attno> <kind> <arg> [w]     kind: like | nlike | regexp | len
//!     like/nlike <pattern>, regexp <pattern> (replacement `\1`), len (octet + char)

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use gravity_hosts::dict::text_hosts::{build_owned, build_table, DictView, TableFn, TableSpec};
use pgrc_format::enc::EncodingId;
use pgrc_read::{DictHandle, MemPartIo, OpenPart, PartExpect, StreamCursor};

fn setup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        postgres_seams::check_for_interrupts::set(|| Ok(()));
        regex_core::init_seams();
    });
    mbutils::SetDatabaseEncoding(wchar::PG_UTF8).unwrap();
    regexp_alt::set_regex_engine(guc_tables::consts::REGEX_ENGINE_RE2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (dir, attno, kind, arg) = (args.get(1).expect("dir"), args.get(2).expect("attno").parse::<u32>().unwrap(), args.get(3).expect("kind").as_str(), args.get(4).cloned().unwrap_or_default());
    let w: usize = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(16);
    let mut files: Vec<_> = std::fs::read_dir(dir).expect("table dir").filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("part-") && n.to_string_lossy().ends_with(".pgrc"))).collect();
    files.sort();
    let parts: Vec<Arc<OpenPart>> = files.iter().enumerate().map(|(i, p)| Arc::new(OpenPart::open(Box::new(MemPartIo::new(std::fs::read(p).expect("part file"), 7, 1000 + i as u64)), &PartExpect::none()).expect("open part"))).collect();
    let binding = pgrc_am::scan::codec_binding();
    let spec = match kind {
        "like" | "nlike" => TableSpec { over: 0, func: TableFn::Like { pattern: arg.clone().into_bytes(), ilike: false, negated: kind == "nlike" } },
        "regexp" => TableSpec { over: 0, func: TableFn::RegexpReplace { pattern: arg.clone().into_bytes(), replacement: b"\\1".to_vec(), collation: 950, encoding: 6, global: false } },
        "len" => TableSpec { over: 0, func: TableFn::OctetLength },
        _ => panic!("kind"),
    };
    let next = AtomicUsize::new(0);
    let (entries, mismatches, verbatim, build_ns, oracle_ns, pair_ns, finder_ns, stream_ns) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..w {
            s.spawn(|| {
                setup();
                let (mut fmgr, mut region_buf) = (None, vec![0u64; 1 << 16]);
                loop {
                    let pi = next.fetch_add(1, Ordering::Relaxed);
                    let Some(part) = parts.get(pi) else { break };
                    let cur = StreamCursor::open(Arc::clone(part), binding, attno, 0).expect("cursor");
                    if cur.entry().encoding != EncodingId::DictCodes as u16 {
                        verbatim.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let d = DictHandle::open(Arc::clone(part), None, binding.unwrappers, attno, 0).expect("dict");
                    let err = core::cell::Cell::new(None);
                    let view = DictView::Part(&d, &err);
                    let n = d.ncodes() as usize;
                    let tb = Instant::now();
                    let table = build_owned(n, d.resident_payload_bytes() as usize, &mut |r| build_table(&spec, &view, r, &mut fmgr)).unwrap_or_else(|e| panic!("build: {e:?}"));
                    build_ns.fetch_add(tb.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    let hdr = table.header();
                    let mut bad = 0u64;
                    let to = Instant::now();
                    let mut it = d.entries(0, n as u32).expect("entries");
                    let mut ents: Vec<(u32, Vec<u8>)> = Vec::with_capacity(n);
                    while let Some((code, e)) = it.next_entry().expect("entry") {
                        ents.push((code, e.bytes.to_vec()));
                    }
                    match kind {
                        "like" | "nlike" => {
                            let bits = unsafe { gravity_dict::Bits::at(hdr) };
                            for (&code, b) in ents.iter().map(|(c, b)| (c, b.as_slice())) {
                                let want = (adt_like::utf8_match_text(b, arg.as_bytes(), None).unwrap() == adt_like::LIKE_TRUE) != (kind == "nlike");
                                if unsafe { bits.get(code) } != want {
                                    bad += 1;
                                    if bad <= 3 {
                                        eprintln!("part {pi} code {code}: LIKE {arg:?} on {:?}: table {} oracle {want}", String::from_utf8_lossy(b), !want);
                                    }
                                }
                            }
                        }
                        "regexp" => {
                            let drv = unsafe { gravity_dict::Derived::at(hdr) };
                            let cx = mcx::MemoryContext::new("textfn_diff");
                            let re = regexp_alt::dispatch(arg.as_bytes(), regex_spencer::REG_ADVANCED, b"x").unwrap().expect("re2");
                            for (&code, b) in ents.iter().map(|(c, b)| (c, b.as_slice())) {
                                let want = regexp_alt::replace_text_regexp_re2(cx.mcx(), &re, b, b"\\1", 0, 1).unwrap();
                                if unsafe { drv.view(code).bytes() } != want.as_slice() {
                                    bad += 1;
                                    if bad <= 3 {
                                        eprintln!("part {pi} code {code}: regexp on {:?}: table {:?} oracle {:?}", String::from_utf8_lossy(b), String::from_utf8_lossy(unsafe { drv.view(code).bytes() }), String::from_utf8_lossy(want.as_slice()));
                                    }
                                }
                            }
                        }
                        _ => {
                            let lens = unsafe { gravity_dict::I64::at(hdr) };
                            let cspec = TableSpec { over: 0, func: TableFn::CharLength };
                            let ctable = build_owned(n, 0, &mut |r| build_table(&cspec, &view, r, &mut fmgr)).unwrap();
                            let chars = unsafe { gravity_dict::I64::at(ctable.header()) };
                            for (&code, b) in ents.iter().map(|(c, b)| (c, b.as_slice())) {
                                let want_c = b.iter().filter(|&&c| c & 0xC0 != 0x80).count() as i64;
                                if unsafe { lens.get(code) } != b.len() as i64 || unsafe { chars.get(code) } != want_c {
                                    bad += 1;
                                    if bad <= 3 {
                                        eprintln!("part {pi} code {code}: lengths {} / {} vs {} / {want_c}", unsafe { lens.get(code) }, unsafe { chars.get(code) }, b.len());
                                    }
                                }
                            }
                        }
                    }
                    oracle_ns.fetch_add(to.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    if matches!(kind, "like" | "nlike") && arg.len() > 2 && arg.starts_with('%') && arg.ends_with('%') && !arg[1..arg.len() - 1].contains(['%', '_', '\\']) {
                        let needle = &arg.as_bytes()[1..arg.len() - 1];
                        let pair = gravity_dict::substr::RarePair::new(needle).unwrap();
                        let finder = memchr::memmem::Finder::new(needle);
                        let t = Instant::now();
                        let mut a = 0u64;
                        for b in ents.iter().map(|(_, b)| b.as_slice()) {
                            a += pair.contains(needle, b) as u64;
                        }
                        pair_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        let t = Instant::now();
                        let mut c = 0u64;
                        for b in ents.iter().map(|(_, b)| b.as_slice()) {
                            c += finder.find(b).is_some() as u64;
                        }
                        finder_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        assert_eq!(a, c, "part {pi}: pair vs finder hit counts");
                        if let Some(payload) = d.payload_bytes().unwrap() {
                            let t = Instant::now();
                            let mut region = unsafe { gravity_dict::Region::new(region_buf.as_mut_ptr() as *mut u8, region_buf.len() * 8) };
                            let m = gravity_dict::Matcher::new(arg.as_bytes(), gravity_dict::Mode::Like, &mut region).unwrap();
                            let idx = HitsIndex { index: d.index_bytes().unwrap() };
                            let bits = m.build_bits_over(&idx, payload, &mut region).unwrap_or_else(|e| panic!("region: {e:?}"));
                            let hits = (0..n as u32).filter(|&c| unsafe { bits.get(c) }).count() as u64;
                            stream_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                            assert_eq!(hits, a, "part {pi}: streamed hits");
                        }
                    }
                    entries.fetch_add(n as u64, Ordering::Relaxed);
                    mismatches.fetch_add(bad, Ordering::Relaxed);
                }
            });
        }
    });
    let ms = |a: &AtomicU64| a.load(Ordering::Relaxed) as f64 / 1e6 / w as f64;
    println!("{kind} {arg:?} attno {attno}: {} parts ({} verbatim skipped), {} entries, {} mismatches, wall {:.0} ms; per-thread ms: build {:.0}, oracle {:.0}", parts.len(), verbatim.load(Ordering::Relaxed), entries.load(Ordering::Relaxed), mismatches.load(Ordering::Relaxed), t0.elapsed().as_secs_f64() * 1e3, ms(&build_ns), ms(&oracle_ns));
    if pair_ns.load(Ordering::Relaxed) > 0 {
        println!("contains A/B (per-thread ms over the same entries): rare-pair per entry {:.0}, memchr Finder per entry {:.0}, rare-pair over the payload (the table build) {:.0}", ms(&pair_ns), ms(&finder_ns), ms(&stream_ns));
    }
    if mismatches.load(Ordering::Relaxed) != 0 {
        std::process::exit(1);
    }
}

struct HitsIndex<'a> {
    index: &'a [u8],
}

impl gravity_dict::EntryIndex for HitsIndex<'_> {
    fn ncodes(&self) -> u32 {
        (self.index.len() / 12) as u32
    }

    fn span(&self, code: u32) -> (usize, usize) {
        let at = code as usize * 12;
        (u32::from_le_bytes(self.index[at..at + 4].try_into().unwrap()) as usize + 4, u32::from_le_bytes(self.index[at + 4..at + 8].try_into().unwrap()) as usize)
    }
}
