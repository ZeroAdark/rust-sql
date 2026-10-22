#![allow(dead_code)]

use pgrc_write::format::sortkey::{NullsOrder, SortDir};
use pgrc_write::structural::ClusterKeyDecl;
use std::cmp::Ordering;

pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
    pub fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

/// The number of cases: `K1_CASES`, else `default`.
pub fn cases(default: u64) -> u64 {
    std::env::var("K1_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub fn random_decl(r: &mut Rng, attno: u32) -> ClusterKeyDecl {
    ClusterKeyDecl { attno, dir: if r.chance(50) { SortDir::Asc } else { SortDir::Desc }, nulls: if r.chance(50) { NullsOrder::First } else { NullsOrder::Last }, path: Vec::new() }
}

/// PG order of one nullable int8 key cell under `d`, written without the seal's comparator.
pub fn ref_cmp(a: Option<i64>, b: Option<i64>, d: &ClusterKeyDecl) -> Ordering {
    let first = d.nulls == NullsOrder::First;
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => if first { Ordering::Less } else { Ordering::Greater },
        (Some(_), None) => if first { Ordering::Greater } else { Ordering::Less },
        (Some(x), Some(y)) => if d.dir == SortDir::Asc { x.cmp(&y) } else { y.cmp(&x) },
    }
}
