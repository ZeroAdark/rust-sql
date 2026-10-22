//! H09's fit: a by-value i32 lane's gather of a granule's survivors at their rows (the hosts' own selective decode:
//! BYTE_FOR's `sel_at_framed`, any other encoding's `decode_sel` and a scatter) against its whole decode
//! (`decode_narrow`), over synthetic lanes at the dev seed (never a benchmark's statements, the census or a bar cell).
//! It prints, per lane, ns per call by survivor count, the least-squares cost per survivor, the whole decode's cost per
//! row, `ceil(per survivor / per row)` and the measured crossover (the survivor share where the gather's time reaches the
//! whole decode's). Run on a box (never the Mac), optimized:
//!   cargo test --profile fast-profile -p pgrc_codec --lib select_fit -- --ignored --nocapture

use super::*;
use crate::bytefor::{sel_at_framed, ByteForEncoder, Framed};
use crate::deltafor::DeltaForEncoder;
use crate::ffor::FforEncoder;
use pgrc_format::abi::NarrowOut;
use pgrc_format::enc::EncodingId;
use pgrc_format::geom::FRAME_VALUES;
use pgrc_format::verbatim::VerbatimEncoder;
use std::hint::black_box;
use std::time::Instant;

const DEV_SEED: u64 = 0x5eed_0de7;
const ROWS: u32 = 8192;

fn ns_per_call(mut f: impl FnMut() -> u64) -> f64 {
    let mut reps = 1u64;
    loop {
        let t = Instant::now();
        for _ in 0..reps {
            black_box(f());
        }
        if t.elapsed().as_millis() >= 20 {
            break;
        }
        reps *= 2;
    }
    (0..5)
        .map(|_| {
            let t = Instant::now();
            for _ in 0..reps {
                black_box(f());
            }
            t.elapsed().as_nanos() as f64 / reps as f64
        })
        .fold(f64::INFINITY, f64::min)
}

fn least_squares(xs: &[f64], ys: &[f64]) -> (f64, f64) {
    let n = xs.len() as f64;
    let (mx, my) = (xs.iter().sum::<f64>() / n, ys.iter().sum::<f64>() / n);
    let sxy: f64 = xs.iter().zip(ys).map(|(x, y)| (x - mx) * (y - my)).sum();
    let sxx: f64 = xs.iter().map(|x| (x - mx) * (x - mx)).sum();
    let slope = sxy / sxx;
    (slope, my - slope * mx)
}

fn survivors(n: usize, seed: &mut u64) -> Vec<u16> {
    let mut all: Vec<u16> = (0..ROWS as u16).collect();
    for i in 0..n {
        let j = i + (splitmix(seed) % (ROWS as u64 - i as u64)) as usize;
        all.swap(i, j);
    }
    let mut s = all[..n].to_vec();
    s.sort_unstable();
    s
}

fn clustered(n: usize, frames: usize, seed: &mut u64) -> Vec<u16> {
    let span = (frames * FRAME_VALUES as usize).min(ROWS as usize);
    let mut all: Vec<u16> = (0..span as u16).collect();
    for i in 0..n.min(span) {
        let j = i + (splitmix(seed) % (span as u64 - i as u64)) as usize;
        all.swap(i, j);
    }
    let mut s = all[..n.min(span)].to_vec();
    s.sort_unstable();
    s
}

fn lane(enc: &mut dyn GranuleEncoder, width: u8, modu: u64, sorted: bool, seed: &mut u64) -> Built {
    let class = StorageClass::ByvalWord { width, signed: true };
    let mut datums: Vec<u64> = (0..ROWS).map(|_| splitmix(seed) % (modu + 1)).collect();
    if sorted {
        datums.sort_unstable();
    }
    let g = GranuleData { rows: ROWS, datums, validity: None };
    build_stream(enc, &[g.input(class)], 0, true)
}

fn scatter<T: Copy>(out: &mut [T], sel: &[u16], dense: &[u64], f: impl Fn(u64) -> T) {
    for (i, &r) in sel.iter().enumerate() {
        out[r as usize] = f(dense[i]);
    }
}

struct Outs {
    dense: Vec<u64>,
    arena: Vec<u8>,
    i16s: Vec<i16>,
    i32s: Vec<i32>,
    words: Vec<u64>,
}

fn gather(built: &Built, width: u8, sel: &[u16], o: &mut Outs) -> u64 {
    let ctx = built.ctx(0);
    let s = Selection { rows: sel };
    if built.key().encoding == EncodingId::ByteFor as u16 {
        let f = Framed::of_ctx(&ctx, EncodingId::ByteFor as u16).expect("framed");
        let w = built.key().width;
        return match width {
            2 => sel_at_framed::<i16>(&f, w, &s, &mut o.i16s),
            4 => sel_at_framed::<i32>(&f, w, &s, &mut o.i32s),
            _ => sel_at_framed::<u64>(&f, w, &s, &mut o.words),
        }
        .expect("sel_at_framed") as u64;
    }
    let vt = crate::registry().resolve(ctx.key).expect("kernel");
    let n = sel.len();
    let mut d = DecodeOut { datums: &mut o.dense[..n], arena: ByteArena::new(&mut o.arena) };
    (vt.decode_sel)(&ctx, &s, &mut d).expect("decode_sel");
    match width {
        2 => scatter(&mut o.i16s, sel, &o.dense, |d| d as i16),
        4 => scatter(&mut o.i32s, sel, &o.dense, |d| d as i32),
        _ => scatter(&mut o.words, sel, &o.dense, |d| d),
    }
    n as u64
}

fn whole(built: &Built, width: u8, o: &mut Outs) -> u64 {
    let ctx = built.ctx(0);
    let vt = crate::registry().resolve(ctx.key).expect("kernel");
    match width {
        2 => (vt.decode_narrow)(&ctx, NarrowOut::I16(&mut o.i16s)).expect("decode_narrow") as u64,
        4 => (vt.decode_narrow)(&ctx, NarrowOut::I32(&mut o.i32s)).expect("decode_narrow") as u64,
        _ => {
            let mut d = DecodeOut { datums: &mut o.words, arena: ByteArena::new(&mut o.arena) };
            (vt.decode_full)(&ctx, &mut d).expect("decode_full") as u64
        }
    }
}

#[test]
#[ignore]
fn select_fit() {
    let mut seed = DEV_SEED;
    let mut lanes: Vec<(String, u8, Built)> = Vec::new();
    for (lw, w, modu) in [(2u8, 1u8, 200u64), (2, 2, 30_000), (4, 1, 200), (4, 2, 60_000), (4, 3, (1 << 24) - 1), (4, 4, (1 << 31) - 1), (8, 1, 200), (8, 2, 60_000), (8, 4, (1 << 31) - 1), (8, 5, (1 << 40) - 1), (8, 8, u64::MAX / 4)] {
        lanes.push((format!("bytefor.i{}.w{w}", lw * 8), lw, lane(&mut ByteForEncoder::new_bytefor(lw, w, true), lw, modu, false, &mut seed)));
    }
    for (lw, bits) in [(2u8, 9u32), (4, 4), (4, 12), (4, 20), (4, 31), (8, 40)] {
        lanes.push((format!("ffor.i{}.b{bits}", lw * 8), lw, lane(&mut FforEncoder { signed: true, carry: None }, lw, (1u64 << bits) - 1, false, &mut seed)));
    }
    lanes.push(("deltafor.i32.sorted".to_string(), 4, lane(&mut DeltaForEncoder::default(), 4, (1 << 31) - 1, true, &mut seed)));
    for lw in [4u8, 8] {
        let class = StorageClass::ByvalWord { width: lw, signed: true };
        lanes.push((format!("verbatim.i{}", lw * 8), lw, lane(&mut VerbatimEncoder { class }, lw, (1 << 30) - 1, false, &mut seed)));
    }
    let mut o = Outs { dense: vec![0; ROWS as usize], arena: vec![0; 1 << 16], i16s: vec![0; ROWS as usize], i32s: vec![0; ROWS as usize], words: vec![0; ROWS as usize] };
    println!("lane\tkey\tframes\tsurvivors\tgather_ns\twhole_ns");
    let mut summary = Vec::new();
    for (name, lw, built) in &lanes {
        let full = ns_per_call(|| whole(built, *lw, &mut o));
        let (mut xs, mut ys) = (Vec::new(), Vec::new());
        let mut cross = None;
        for div in [128usize, 64, 32, 24, 16, 12, 10, 8, 6, 5, 4, 3, 2] {
            let n = ROWS as usize / div;
            let sel = survivors(n, &mut seed);
            let g = ns_per_call(|| gather(built, *lw, &sel, &mut o));
            println!("{name}\t{:?}\t8\t{n}\t{g:.1}\t{full:.1}", built.key());
            if div <= 64 {
                xs.push(n as f64);
                ys.push(g);
            }
            if cross.is_none() && g >= full {
                cross = Some(div);
            }
        }
        let mut per_frame = Vec::new();
        for frames in [1usize, 2, 4] {
            for n in [64usize, 256] {
                let sel = clustered(n, frames, &mut seed);
                let g = ns_per_call(|| gather(built, *lw, &sel, &mut o));
                println!("{name}\t{:?}\t{frames}\t{n}\t{g:.1}\t{full:.1}", built.key());
                if n == 64 {
                    per_frame.push(format!("{frames}f {g:.0}"));
                }
            }
        }
        let (per_survivor, fixed) = least_squares(&xs, &ys);
        let per_row = full / ROWS as f64;
        summary.push(format!(
            "{name}\twhole {full:.0} ns ({per_row:.4} ns/row)\tgather {per_survivor:.4} ns/survivor + {fixed:.0} ns\tdiv {}\tloses from 1/{}\t64 survivors in {}",
            (per_survivor / per_row).ceil(),
            cross.map_or("none".to_string(), |d| d.to_string()),
            per_frame.join(", ")
        ));
    }
    for l in summary {
        println!("FIT\t{l}");
    }
}
