use crate::ns::Ns;

const ROWS_PER_GRANULE: f64 = 8192.0;
const GRANULES_MEASURED: (f64, f64) = (1536.0, 12902.4);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TopForms {
    pub rows: f64,
    pub groups: f64,
    pub lead_keys: f64,
    pub lead_spread: Option<f64>,
    pub keys: f64,
    pub key_spread: Option<f64>,
    pub cand: f64,
    pub n: f64,
    pub extreme: bool,
    pub one_holds: bool,
    pub pass1_holds: bool,
    pub spill_priced: bool,
    pub one_spill_mib: f64,
    pub pass1_spill_mib: f64,
    pub width: u32,
}

fn spill_ns(f: &str, w: u32, spread: Option<f64>) -> Option<f64> {
    let (sc, cl) = match f {
        "one" => ("top.c.spill_one_s", "top.c.spill_one_c"),
        "pass1" => ("top.c.spill_pass1_s", "top.c.spill_pass1_c"),
        _ => return None,
    };
    let s = spread.unwrap_or(1.0).clamp(0.0, 1.0);
    let (a, b) = (crate::measured::ns(sc, w)?.max(0.0), crate::measured::ns(cl, w)?.max(0.0));
    Some(b + (a - b) * s)
}

fn name(f: &str, layout: char, tier: &str) -> Option<&'static str> {
    Some(match (f, layout, tier) {
        ("one", 's', "1m") => "top.c.one_s1m",
        ("one", 's', "16m") => "top.c.one_s16m",
        ("one", 's', "u") => "top.c.one_su",
        ("one", 'c', "1m") => "top.c.one_c1m",
        ("one", 'c', "16m") => "top.c.one_c16m",
        ("one", 'c', "u") => "top.c.one_cu",
        ("onew", 's', "1m") => "top.c.onew_s1m",
        ("onew", 's', "16m") => "top.c.onew_s16m",
        ("onew", 's', "u") => "top.c.onew_su",
        ("onew", 'c', "1m") => "top.c.onew_c1m",
        ("onew", 'c', "16m") => "top.c.onew_c16m",
        ("onew", 'c', "u") => "top.c.onew_cu",
        ("two", 's', "1m") => "top.c.two_s1m",
        ("two", 's', "16m") => "top.c.two_s16m",
        ("two", 's', "u") => "top.c.two_su",
        ("two", 'c', "1m") => "top.c.two_c1m",
        ("two", 'c', "16m") => "top.c.two_c16m",
        ("two", 'c', "u") => "top.c.two_cu",
        ("key", 's', "1m") => "top.c.key_s1m",
        ("key", 's', "16m") => "top.c.key_s16m",
        ("key", 's', "u") => "top.c.key_su",
        ("key", 'c', "1m") => "top.c.key_c1m",
        ("key", 'c', "16m") => "top.c.key_c16m",
        ("key", 'c', "u") => "top.c.key_cu",
        _ => return None,
    })
}

const PER_KEY: [(f64, &str); 3] = [(1.0, "u"), (6.0, "16m"), (96.0, "1m")];

fn form_on(f: &str, w: u32, tiers: &[(f64, &'static str); 3], x: f64, spread: Option<f64>, gr: f64) -> Option<f64> {
    let s = spread.unwrap_or(1.0).clamp(0.0, 1.0);
    let line = |r: crate::measured::Row| if f == "key" { r.ns } else { (r.fixed_ns + r.ns * gr).max(0.0) };
    let at = |tier: &str| -> Option<f64> {
        let (sc, cl) = (line(crate::measured::at(name(f, 's', tier)?, w)?), line(crate::measured::at(name(f, 'c', tier)?, w)?));
        Some(cl + (sc - cl) * s)
    };
    let k = x.clamp(tiers[0].0, tiers[2].0);
    let i = if k <= tiers[1].0 { 0 } else { 1 };
    let (a, b) = (tiers[i], tiers[i + 1]);
    let t = (k.ln() - a.0.ln()) / (b.0.ln() - a.0.ln());
    let (pa, pb) = (at(a.1)?, at(b.1)?);
    Some(pa + (pb - pa) * t)
}

fn form(f: &str, w: u32, keys: f64, spread: Option<f64>, gr: f64) -> Option<f64> {
    form_on(f, w, &PER_KEY, gr * ROWS_PER_GRANULE / keys.max(1.0), spread, gr)
}

fn two_none(w: u32, per_key: f64, spread: Option<f64>, gr: f64) -> Option<f64> {
    let s = spread.unwrap_or(1.0).clamp(0.0, 1.0);
    let at = |tier: &str| -> Option<f64> {
        let mut v = [0.0f64; 2];
        for (j, l) in ['s', 'c'].into_iter().enumerate() {
            let line = |f: &str| crate::measured::at(name(f, l, tier)?, w).map(|r| (r.fixed_ns + r.ns * gr).max(0.0));
            let all = line("two")?;
            v[j] = if tier == "u" { all } else { (all - (line("onew")? - line("one")?).max(0.0)).max(0.0) };
        }
        Some(v[1] + (v[0] - v[1]) * s)
    };
    let k = per_key.clamp(PER_KEY[0].0, PER_KEY[2].0);
    let i = if k <= PER_KEY[1].0 { 0 } else { 1 };
    let (a, b) = (PER_KEY[i], PER_KEY[i + 1]);
    let t = (k.ln() - a.0.ln()) / (b.0.ln() - a.0.ln());
    let (pa, pb) = (at(a.1)?, at(b.1)?);
    Some(pa + (pb - pa) * t)
}

fn one_pass(w: u32, groups: f64, keys: f64, spread: Option<f64>, gr: f64) -> Option<f64> {
    let keys = keys.max(1.0);
    let (lo, hi) = (form("one", w, keys, spread, gr)?, form("onew", w, keys, spread, gr)?);
    let rows = gr * ROWS_PER_GRANULE;
    let span = (rows.ln() - keys.min(rows).ln()).max(f64::EPSILON);
    let t = ((groups.max(keys).ln() - keys.ln()) / span).clamp(0.0, 1.0);
    Some(lo + (hi - lo) * t)
}

pub fn forms(t: &TopForms) -> Option<(Ns, Ns)> {
    let (w, gr) = (t.width, t.rows.max(0.0) / ROWS_PER_GRANULE);
    let spilled = !t.one_holds || !t.pass1_holds;
    if t.extreme || (spilled && !t.spill_priced) || !(GRANULES_MEASURED.0..=GRANULES_MEASURED.1).contains(&gr) {
        return None;
    }
    let one_spill = if t.one_holds { 0.0 } else { spill_ns("one", w, t.lead_spread)? * t.one_spill_mib.max(0.0) };
    let pass1_spill = if t.pass1_holds { 0.0 } else { spill_ns("pass1", w, t.key_spread)? * t.pass1_spill_mib.max(0.0) };
    let one = one_pass(w, t.groups, t.lead_keys, t.lead_spread, gr)? + one_spill;
    let per_key = t.rows.max(1.0) / t.keys.max(1.0);
    let none = two_none(w, per_key, t.key_spread, gr)?;
    let fold = (form("onew", w, t.keys, t.key_spread, gr)? - form("one", w, t.keys, t.key_spread, gr)?).max(0.0);
    let share = (t.cand / t.rows.max(1.0)).clamp(0.0, 1.0);
    let key = form_on("key", w, &PER_KEY, per_key, t.key_spread, gr).unwrap_or(0.0);
    let two = none + fold * share + key * t.n.max(0.0) + pass1_spill;
    Some((Ns(one), Ns(two)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn top(keys: f64, cand_share: f64, spread: f64, width: u32) -> TopForms {
        let rows = 1e8;
        TopForms { rows, groups: 9e7, lead_keys: keys, lead_spread: Some(spread), keys, key_spread: Some(spread), cand: rows * cand_share, n: 10.0, extreme: false, one_holds: true, pass1_holds: true, spill_priced: true, one_spill_mib: 0.0, pass1_spill_mib: 0.0, width }
    }

    #[test]
    fn the_forms_read_the_measured_table() {
        for w in [0, 2, 8] {
            for s in [0.0, 0.5, 1.0] {
                let (one, two) = forms(&top(2e7, 1.0, s, w)).expect("the top corpus is measured");
                assert_eq!(forms(&TopForms { rows: 3e6, ..top(2e7, 1.0, s, w) }), None, "under the corpus' sizes");
                assert!(one.0 > 0.0 && two.0 > 0.0, "W{w} spread {s}: {one:?} {two:?}");
            }
        }
        assert_eq!(forms(&TopForms { extreme: true, ..top(1e8, 0.0, 1.0, 8) }), None, "extremes are not measured");
    }

    #[test]
    fn a_spilled_form_is_its_held_form_plus_its_measured_spill() {
        for w in [0, 2, 8] {
            let held = top(1e8, 0.0, 1.0, w);
            let (one, two) = forms(&held).expect("the top corpus is measured");
            let p1 = TopForms { pass1_holds: false, pass1_spill_mib: 1000.0, ..held };
            assert_eq!(forms(&TopForms { spill_priced: false, ..p1 }), None, "W{w}: off, a spilled pass 1 has no measured price");
            assert_eq!(forms(&TopForms { one_holds: false, one_spill_mib: 1000.0, spill_priced: false, ..held }), None, "W{w}: off");
            match (spill_ns("pass1", w, Some(1.0)), spill_ns("one", w, Some(1.0))) {
                (Some(p), Some(o)) => {
                    let (one1, two1) = forms(&p1).expect("pass 1's spill is measured");
                    assert_eq!(one1, one, "W{w}: the one pass held");
                    assert!((two1.0 - (two.0 + 1000.0 * p)).abs() < 1e-6 * two1.0.max(1.0), "W{w}: two passes + 1000 MiB of pass 1's spill");
                    let (one2, _) = forms(&TopForms { one_holds: false, one_spill_mib: 1000.0, ..held }).expect("the one pass's spill is measured");
                    assert!((one2.0 - (one.0 + 1000.0 * o)).abs() < 1e-6 * one2.0.max(1.0), "W{w}: one pass + 1000 MiB of its spill");
                }
                _ => assert_eq!(forms(&p1), None, "W{w}: no spill row measured, no price"),
            }
        }
    }
}
