//! Canonical dump of a parquet file for the pyarrow differential
//! (`scripts/parquet-differential.py`): one line per leaf slot,
//! `<col>\t<r><rep>d<def>\t<value>`, values rendered by physical type so
//! both sides print byte-exact text (floats as bit patterns, bytes as hex,
//! INT96 as Unix nanoseconds, decimals as their unscaled integer).
//! Errors print `ERROR\t<message>` and exit 2.

use std::io::Write;

use parquet_read::{BatchData, ColumnBatch, FileReader, Logical, Phys};

/// Big-endian two's complement of any width -> decimal digits (DECIMAL on
/// FLBA/BYTE_ARRAY is not bounded by i128: precision 50 stores 22 bytes).
fn unscaled_be(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "0".to_string();
    }
    let negative = bytes[0] & 0x80 != 0;
    let mut limbs: Vec<u32> = Vec::with_capacity(bytes.len() / 4 + 1);
    let mut carry: u64 = 1;
    for chunk in bytes.rchunks(4) {
        let mut w = [if negative { 0xffu8 } else { 0 }; 4];
        w[4 - chunk.len()..].copy_from_slice(chunk);
        let mut v = u32::from_be_bytes(w);
        if negative {
            let x = u64::from(!v) + carry;
            v = x as u32;
            carry = x >> 32;
        }
        limbs.push(v);
    }
    let mut groups: Vec<u32> = Vec::new();
    while limbs.iter().any(|&l| l != 0) {
        let mut rem: u64 = 0;
        for l in limbs.iter_mut().rev() {
            let cur = (rem << 32) | u64::from(*l);
            *l = (cur / 1_000_000_000) as u32;
            rem = cur % 1_000_000_000;
        }
        groups.push(rem as u32);
    }
    if groups.is_empty() {
        return "0".to_string();
    }
    let mut s = String::with_capacity(groups.len() * 9 + 1);
    if negative {
        s.push('-');
    }
    s.push_str(&groups.last().expect("non-empty").to_string());
    for g in groups.iter().rev().skip(1) {
        s.push_str(&format!("{g:09}"));
    }
    s
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: pqdump <file.parquet> [batch]");
        std::process::exit(1);
    }
    let batch_n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1000);
    // Optional third argument: read only this leaf ordinal.
    let only: Option<usize> = args.get(3).and_then(|s| s.parse().ok());
    let out = std::io::stdout();
    let mut w = std::io::BufWriter::new(out.lock());
    let mut fr = match FileReader::open(&args[1]) {
        Ok(f) => f,
        Err(e) => {
            let _ = writeln!(w, "ERROR\t{}", e.message);
            let _ = w.flush();
            std::process::exit(2);
        }
    };
    let ncols = fr.meta.columns.len();
    let cols: Vec<usize> = match only {
        Some(c) => vec![c],
        None => (0..ncols).collect(),
    };
    let vutf8: Vec<bool> = cols
        .iter()
        .map(|&i| {
            let c = &fr.meta.columns[i];
            c.phys == Phys::ByteArray && matches!(c.logical, Logical::String | Logical::Enum | Logical::Json)
        })
        .collect();
    let mut batches: Vec<ColumnBatch> =
        cols.iter().map(|&i| ColumnBatch::new_for(fr.meta.columns[i].phys)).collect();
    let meta = fr.meta_arc();
    for rg in 0..meta.row_groups.len() {
        let mut rgr = match fr.row_group(rg, &cols, &vutf8) {
            Ok(r) => r,
            Err(e) => {
                let _ = writeln!(w, "ERROR\t{}", e.message);
                let _ = w.flush();
                std::process::exit(2);
            }
        };
        while rgr.rows_remaining() > 0 {
            let n = rgr.rows_remaining().min(batch_n as u64) as usize;
            if let Err(e) = rgr.read_batches(&mut batches, n) {
                let _ = writeln!(w, "ERROR\t{} {}", e.message, e.detail.as_deref().unwrap_or(""));
                let _ = w.flush();
                std::process::exit(2);
            }
            for (c, b) in batches.iter().enumerate() {
                let schema = &meta.columns[cols[c]];
                let lv = rgr.levels(c);
                let nested = !lv.def.is_empty();
                for k in 0..b.len() {
                    let _ = write!(w, "{}\t", schema.name);
                    if nested {
                        let _ = write!(w, "r{}d{}\t", lv.rep[k], lv.def[k]);
                    } else {
                        let _ = write!(w, "r0d{}\t", if b.is_null(k) { 0 } else { schema.max_def });
                    }
                    if b.is_null(k) {
                        let _ = writeln!(w, "NULL");
                        continue;
                    }
                    match &b.data {
                        BatchData::Bool(v) => {
                            let _ = writeln!(w, "{}", if v[k] { "t" } else { "f" });
                        }
                        BatchData::I32(v) => {
                            let _ = writeln!(w, "{}", v[k]);
                        }
                        BatchData::I64(v) => {
                            let _ = writeln!(w, "{}", v[k]);
                        }
                        BatchData::F32(v) => {
                            let _ = writeln!(w, "f{:08x}", v[k].to_bits());
                        }
                        BatchData::F64(v) => {
                            let _ = writeln!(w, "d{:016x}", v[k].to_bits());
                        }
                        BatchData::I96(v) => {
                            // Arrow's formula (unsigned domain, wrapping), so
                            // garbage dates render identically on both sides.
                            let [lo, hi, jd] = v[k];
                            let nanos = u64::from(hi) << 32 | u64::from(lo);
                            let days = u64::from(jd).wrapping_sub(2_440_588);
                            let ns = days.wrapping_mul(86_400_000_000_000).wrapping_add(nanos) as i64;
                            let _ = writeln!(w, "{ns}");
                        }
                        BatchData::Bytes { .. } => {
                            let bytes = b.bytes_at(k);
                            match schema.logical {
                                Logical::Decimal { .. } => {
                                    let _ = writeln!(w, "{}", unscaled_be(bytes));
                                }
                                Logical::Float16 if bytes.len() == 2 => {
                                    // NaN payloads are not a value: both sides render "hnan".
                                    let bits = u16::from_le_bytes([bytes[0], bytes[1]]);
                                    if bits & 0x7c00 == 0x7c00 && bits & 0x3ff != 0 {
                                        let _ = writeln!(w, "hnan");
                                    } else {
                                        let _ = writeln!(w, "h{bits:04x}");
                                    }
                                }
                                _ => {
                                    let mut s = String::with_capacity(bytes.len() * 2 + 1);
                                    s.push('x');
                                    for &x in bytes {
                                        s.push_str(&format!("{x:02x}"));
                                    }
                                    let _ = writeln!(w, "{s}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let _ = w.flush();
}
