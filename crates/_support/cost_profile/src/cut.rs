use crate::anchor::Anchor;
use crate::fit::Fitted;

const MACHINE: &str = "box 34: c8g.16xlarge, Graviton 4 (Neoverse V2), held whole (PERF-BOX-QUEUE); checked on box 40, c8g.4xlarge";
const CORPUS: &str = "code cut sweep (kernels the_cut_unit_costs; cutfit cf1-cf4, dev seed)";

/// The cut's prices move with the sort anchor: a replace and a pass are both compares and moves of a run's entries.
const fn replace(ns: f64) -> Fitted {
    Fitted { ns, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Sort }
}

pub const CUT_REPLACE: [Fitted; 8] =
    [replace(5.58), replace(5.69), replace(5.81), replace(7.07), replace(10.92), replace(17.00), replace(20.44), replace(25.75)];
pub const CUT_PASS: Fitted = Fitted { ns: 28.86, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Sort };

pub const REST_MAX: u64 = u32::MAX as u64;

pub const fn levels(n: u64) -> u64 {
    if n <= 1 {
        0
    } else {
        63 - n.leading_zeros() as u64
    }
}

pub const fn replace_ns(n: u64) -> f64 {
    let d = levels(n) as usize;
    CUT_REPLACE[if d < CUT_REPLACE.len() { d } else { CUT_REPLACE.len() - 1 }].ns
}

pub const REST_Q16: [u64; 8] = {
    let mut t = [0u64; 8];
    let mut d = 0;
    while d < 8 {
        let q = CUT_REPLACE[d].ns / CUT_PASS.ns * 65536.0;
        let i = q as u64;
        t[d] = if (i as f64) < q { i + 1 } else { i };
        d += 1;
    }
    t
};

pub const fn rest_calls(n: u64, round: u64) -> u64 {
    let d = levels(n) as usize;
    let q = REST_Q16[if d < REST_Q16.len() { d } else { REST_Q16.len() - 1 }];
    let r = q.saturating_mul(round.saturating_add(1)).saturating_add((1 << 17) - 1) >> 17;
    if r == 0 {
        1
    } else if r > REST_MAX {
        REST_MAX
    } else {
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rule_on_the_fit() {
        assert_eq!(CUT_PASS.ns, 28.86);
        let q: Vec<f64> = [1u64, 2, 8, 16, 251].iter().map(|&n| (replace_ns(n) / CUT_PASS.ns * 1000.0).round() / 1000.0).collect(); // alloc: Cold
        assert_eq!(q, [0.193, 0.197, 0.245, 0.378, 0.892]);
        assert_eq!(REST_Q16, [12672, 12921, 13194, 16055, 24798, 38605, 46416, 58474]);
        let rests = |n: u64| [0, 1, 2, 3, 10, 100, 1000, 65535].map(|j| rest_calls(n, j));
        assert_eq!(rests(251), [1, 1, 2, 2, 5, 46, 447, 29237]);
        assert_eq!(rests(1), [1, 1, 1, 1, 2, 10, 97, 6336]);
        let calls = |n: u64| {
            let (mut at, mut taken, mut j) = (0u64, 0u64, 0u64);
            while at < 250_000 {
                (at, taken, j) = (at + 1 + rest_calls(n, j), taken + 1, (j + 1).min(0xffff));
            }
            taken
        };
        assert_eq!([calls(1), calls(2), calls(16), calls(251)], [2259, 2237, 1618, 1055]);
    }

    #[test]
    fn a_rest_is_the_balance_on_the_run_so_far() {
        for n in 1..=251u64 {
            let q = replace_ns(n) / CUT_PASS.ns;
            let mut d = 0.0f64;
            for j in 0..65535 {
                let (r, s) = (rest_calls(n, j), (q * d).sqrt());
                if d >= 4.0 / q {
                    assert!(r as f64 >= 0.6 * s && (r - 1) as f64 <= s, "n {n} round {j}: {r} calls against {s:.1}");
                }
                d += (r + 1) as f64;
            }
            assert_eq!(rest_calls(n, u64::MAX / 4), REST_MAX, "n {n}: the bound");
        }
    }

    #[test]
    fn the_heap_depth_is_floor_log2() {
        assert_eq!([levels(0), levels(1), levels(2), levels(3), levels(4), levels(251), levels(256)], [0, 0, 1, 1, 2, 7, 8]);
    }
}
