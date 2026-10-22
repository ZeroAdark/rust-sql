//! gravity-parallel-scale table B's decode column: the pgrc granule decode
//! of the lanes a shape reads, alone (decode_full + validity per granule,
//! into scratch, no kernel), over the table's part files, on W threads.
//!
//!   decode <table dir> <attnos csv> [w,w,..]      ms per W (median of RUNS), rows decoded

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use pgrc_format::abi::{ByteArena, DecodeOut};
use pgrc_format::geom::GRANULE_ROWS;
use pgrc_read::{MemPartIo, OpenPart, PartExpect, StreamCursor};

const ROWS: usize = GRANULE_ROWS as usize;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).expect("decode <table dir> <attnos> [widths]");
    let attnos: Vec<u32> = args.get(2).expect("attnos").split(',').filter_map(|a| a.parse().ok()).collect();
    let widths: Vec<usize> = args.get(3).map(|s| s.split(',').filter_map(|w| w.parse().ok()).collect()).unwrap_or_else(|| vec![1, 4, 8, 12, 16]);
    let runs: usize = std::env::var("RUNS").ok().and_then(|s| s.parse().ok()).unwrap_or(5);
    let mut files: Vec<_> = std::fs::read_dir(dir).expect("table dir").filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("part-") && n.to_string_lossy().ends_with(".pgrc"))).collect();
    files.sort();
    let t0 = Instant::now();
    let parts: Vec<Arc<OpenPart>> = files
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let bytes = std::fs::read(p).expect("part file");
            Arc::new(OpenPart::open(Box::new(MemPartIo::new(bytes, 7, 1000 + i as u64)), &PartExpect::none()).expect("open part"))
        })
        .collect();
    let rows: u64 = parts.iter().map(|p| p.rows()).sum();
    let granules: Vec<(usize, u32)> = parts.iter().enumerate().flat_map(|(i, p)| (0..p.footer().granule_count).map(move |g| (i, g))).collect();
    eprintln!("{} parts, {} granules, {rows} rows, lanes {attnos:?}, loaded in {:.1} s", parts.len(), granules.len(), t0.elapsed().as_secs_f64());
    let binding = pgrc_am::scan::codec_binding();
    for &w in &widths {
        let mut ms = Vec::new();
        let mut decoded = 0u64;
        for _ in 0..=runs {
            let next = AtomicUsize::new(0);
            let t = Instant::now();
            decoded = std::thread::scope(|s| {
                let hs: Vec<_> = (0..w)
                    .map(|_| {
                        let (parts, granules, attnos, next) = (&parts, &granules, &attnos, &next);
                        s.spawn(move || {
                            let mut datums = vec![0u64; ROWS];
                            let mut valid = vec![0u64; ROWS / 64];
                            let mut arena = [0u64; 8];
                            let mut cur: Option<(usize, Vec<StreamCursor<'_>>)> = None;
                            let mut n = 0u64;
                            loop {
                                let j = next.fetch_add(1, Ordering::Relaxed);
                                let Some(&(pi, g)) = granules.get(j) else { break };
                                if cur.as_ref().is_none_or(|c| c.0 != pi) {
                                    cur = Some((pi, attnos.iter().map(|&a| StreamCursor::open(parts[pi].clone(), binding, a, 0).expect("cursor")).collect()));
                                }
                                for c in cur.as_mut().unwrap().1.iter_mut() {
                                    let rows = c.rows_in_granule(g) as usize;
                                    let bytes = unsafe { core::slice::from_raw_parts_mut(arena.as_mut_ptr() as *mut u8, 64) };
                                    let mut out = DecodeOut { datums: &mut datums[..rows], arena: ByteArena::new(bytes) };
                                    n += c.decode_full(g, &mut out).expect("decode") as u64;
                                    if c.has_validity_stream() {
                                        c.validity(g, &mut valid).expect("validity");
                                    }
                                }
                            }
                            n
                        })
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap()).sum()
            });
            ms.push(t.elapsed().as_secs_f64() * 1e3);
        }
        ms.remove(0);
        ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("| decode {attnos:?} | {w} | {:.1} | {} lane-rows |", ms[ms.len() / 2], decoded);
    }
}
