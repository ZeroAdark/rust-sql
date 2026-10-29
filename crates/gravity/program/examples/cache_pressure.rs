//! Reproducible cache-only benchmark. Program construction is outside timed sections.
//! cargo run --locked --release -p gravity_program --example cache_pressure
use gravity_abi::Ctx;
use gravity_program::{Code, Layout, Program, ProgramCache, WritePlan};
use std::{hint::black_box, sync::Arc, time::Instant};

unsafe extern "C-unwind" fn entry(_: *mut Ctx) -> i64 { 0 }

fn main() {
    println!("case,entries,operations,elapsed_ns,ns_per_op");
    for n in [128, 512, 2048, 8192] {
        let programs: Vec<_> = (0..=n).map(|i| Arc::new(Program::new(
            Layout { nsites: 0, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 },
            &[], &[], WritePlan::SELECT, Code::hand(entry).with_identity(i as u128 + 1),
        ))).collect();
        let mut cache = ProgramCache::new(usize::MAX, n);
        for p in &programs[..n] { cache.insert(None, p.clone()); }
        // Cycle through n+1 keys: exactly one eviction per operation, at a fixed live size.
        let ops = 50_000usize;
        let start = Instant::now();
        for i in 0..ops { cache.insert(None, black_box(programs[(n + i) % (n + 1)].clone())); }
        let ns = start.elapsed().as_nanos();
        println!("churn,{n},{ops},{ns},{:.2}", ns as f64 / ops as f64);
        assert_eq!(cache.l2_len(), n);
        let key = programs[(n + ops - 1) % (n + 1)].key;
        let start = Instant::now();
        for _ in 0..ops { black_box(cache.l2(black_box(key)).unwrap()); }
        let ns = start.elapsed().as_nanos();
        println!("hot_hit,{n},{ops},{ns},{:.2}", ns as f64 / ops as f64);
    }
}
