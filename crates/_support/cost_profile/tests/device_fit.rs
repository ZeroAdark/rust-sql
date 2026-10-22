use cost_profile::fit::{break_even_div, least_squares};
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::time::Instant;

const DEV_SEED: u64 = 0x5eed_0de7;
const FILE_BYTES: u64 = 256 << 20;
const ALIGN: usize = 4096;
#[cfg(target_arch = "aarch64")]
const O_DIRECT: i32 = 0o200000;
#[cfg(not(target_arch = "aarch64"))]
const O_DIRECT: i32 = 0o40000;

struct Buf(*mut u8, Layout);
impl Buf {
    fn new(bytes: usize) -> Buf {
        let l = Layout::from_size_align(bytes, ALIGN).unwrap();
        // SAFETY: a non-zero size with a power-of-two alignment.
        Buf(unsafe { alloc_zeroed(l) }, l)
    }
    fn slice(&mut self) -> &mut [u8] {
        // SAFETY: the allocation is `self.1.size()` bytes, owned by `self`.
        unsafe { std::slice::from_raw_parts_mut(self.0, self.1.size()) }
    }
}
impl Drop for Buf {
    fn drop(&mut self) {
        // SAFETY: allocated by `alloc_zeroed` with this layout.
        unsafe { dealloc(self.0, self.1) }
    }
}

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s >> 12;
    *s ^= *s << 25;
    *s ^= *s >> 27;
    s.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

fn random_reads(f: &File, bytes: usize, n: usize, s: &mut u64) -> f64 {
    let mut b = Buf::new(bytes);
    let slots = (FILE_BYTES as usize - bytes) / ALIGN;
    let t = Instant::now();
    for _ in 0..n {
        let off = (xorshift(s) as usize % slots) * ALIGN;
        f.read_exact_at(b.slice(), off as u64).unwrap();
    }
    t.elapsed().as_nanos() as f64 / n as f64
}

fn sequential(f: &File, chunk: usize) -> f64 {
    let mut b = Buf::new(chunk);
    let t = Instant::now();
    let mut off = 0u64;
    while off + chunk as u64 <= FILE_BYTES {
        f.read_exact_at(b.slice(), off).unwrap();
        off += chunk as u64;
    }
    t.elapsed().as_nanos() as f64 / off as f64
}

#[test]
#[ignore = "a measurement: run on a box with --ignored (module doc)"]
fn device_reads_at_the_dev_seed() {
    let dir = std::env::var("COST_FIT_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    let path = dir.join(format!("cost_fit_device.{}", std::process::id()));
    let mut s = DEV_SEED;
    {
        let mut w = File::create(&path).unwrap();
        let mut chunk = vec![0u8; 1 << 20];
        for _ in 0..FILE_BYTES >> 20 {
            for c in chunk.chunks_exact_mut(8) {
                c.copy_from_slice(&xorshift(&mut s).to_le_bytes());
            }
            w.write_all(&chunk).unwrap();
        }
        w.sync_all().unwrap();
    }
    println!("fit\tmode\tunit\tvalue\tnote");
    let cold = OpenOptions::new().read(true).custom_flags(O_DIRECT).open(&path).unwrap();
    let pts: Vec<(f64, f64)> = [4096usize, 16 << 10, 64 << 10, 256 << 10].iter().map(|&b| (b as f64, random_reads(&cold, b, 256, &mut s))).collect();
    for (b, ns) in &pts {
        println!("fit\tcold\trandom_read_{b}\t{ns:.0}\tns per O_DIRECT read at a random offset");
    }
    let (fixed, per_byte) = least_squares(&pts);
    let seq = sequential(&cold, 1 << 20);
    println!("fit\tcold\tread_fixed_ns\t{fixed:.0}\tleast-squares intercept over 4 KiB .. 256 KiB");
    println!("fit\tcold\trandom_ns_per_byte\t{per_byte:.4}\tleast-squares slope");
    println!("fit\tcold\tsequential_ns_per_byte\t{seq:.4}\t1 MiB reads, whole file");
    println!("fit\tcold\tcoalesce_gap_bytes\t{:.0}\tfixed / sequential ns per byte", fixed / seq);
    let block = 16u64 << 10;
    let (r, q) = (fixed + per_byte * block as f64, seq * block as f64);
    println!("fit\tcold\tstream_div_16k\t{}\tbreak_even_div(random 16 KiB block, streamed 16 KiB)", break_even_div(r, q));
    drop(cold);
    let warm = File::open(&path).unwrap();
    let _ = sequential(&warm, 1 << 20);
    let wseq = sequential(&warm, 1 << 20);
    let wpts: Vec<(f64, f64)> = [4096usize, 16 << 10, 64 << 10, 256 << 10].iter().map(|&b| (b as f64, random_reads(&warm, b, 4096, &mut s))).collect();
    let (wfixed, wper) = least_squares(&wpts);
    println!("fit\twarm\tread_fixed_ns\t{wfixed:.0}\tpage-cache pread intercept");
    println!("fit\twarm\trandom_ns_per_byte\t{wper:.4}\tpage-cache pread slope");
    println!("fit\twarm\tsequential_ns_per_byte\t{wseq:.4}\tpage-cache 1 MiB reads");
    println!("fit\twarm\tcoalesce_gap_bytes\t{:.0}\tfixed / sequential ns per byte", wfixed / wseq);
    println!("fit\twarm\tstream_div_16k\t{}\tbreak_even_div(random 16 KiB block, streamed 16 KiB)", break_even_div(wfixed + wper * block as f64, wseq * block as f64));
    std::fs::remove_file(&path).unwrap();
    assert!(fixed > 0.0 && seq > 0.0, "a read costs something");
}
