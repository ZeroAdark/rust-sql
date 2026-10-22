//! Dense code front, derived key per entry. Hooks / levers: H23/H09 R4-L16 D021. Budget 160 lines (files.tsv, +25 %
//! gate). Owner M1-WRITE (from M1-TABLES, 09-24).
//!
//! The record front of code.front_part (codefront.rs: a record per index of the window, opened with the row's image,
//! listed, merged into the hashed table at every window end) indexed where one plain code key cannot index it
//! (`KeyShape.dense`, `ImageWords.index`):
//! - two or more code keys: the mixed-radix index `c0 + n0 * (c1 + n1 * ..)` over the window's dictionary sizes (each
//!   key's lane's `ncodes`), program cost/dict_front.rs's one rule: every tuple of a window whose dictionary-size product
//!   fits the front's capacity has a record; a code at or past its size (a NULL's all ones) or an index past the
//!   capacity takes the hashed table, so a window past it keeps its low tuples in the front. Three or more keys take
//!   the front when the planner's group estimate fits its capacity (`dict_front::takes`), a pair at any estimate;
//! - one key derived from one code lane (e.g. `regexp_replace(col, ..)`): index the source code. The record keys by
//!   the row's image, which every row of one code shares (an IMMUTABLE / STABLE function of the code: KEYED sets
//!   `dense` for those only); two codes of one value are two records the flush merges.
//! The key itself is never evaluated here: the image is the row's, computed as the hashed path computes it, so a
//! per-entry value (percode.table) is read per row, a faulting entry re-evaluates per row and raises where PostgreSQL
//! raises, and a VOLATILE key never reaches a front. Codes are part-local; a window reads one dictionary per lane, and
//! the window-end flush keeps two parts' codes apart by construction (codefront.rs).
//!
//! A table the plain front takes (one plain code key) is declined, so the resolver passes it to code.front_part; float
//! slots are declined as there (a merge reassociates float partials: V4R-2 keeps overflow exact).
//!
//! Registers: road `code.front_dense` (`CODE_FRONT_DENSE`) as a `TableImpl`, the resolver's first policy.
use gravity_abi::breaker::SlotKind;
use gravity_abi::ctx::site_off;
use gravity_abi::site::site;
use gravity_abi::{lane, LANE_SIZE};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use gravity_program::cost::dict_front;

use crate::dict::codefront::FRONT;
use crate::table::face::{DenseFront, TableDecl, TableImpl, TableMech};
use crate::table::keyimg::{FrontIndex, ImageWords};
use crate::frame::env::Env;
use crate::frame::stop::Exits;

crate::strategies! {
    /// Dense code front, derived key per entry.
    CODE_FRONT_DENSE = 47, "code.front_dense", CodeKeyedPgrc;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(CODE_FRONT_DENSE, &TableImpl(&DENSE))], ..crate::ops::NONE };

pub(crate) struct Dense;
pub(crate) static DENSE: Dense = Dense;

impl Dense {
    /// The row's front index: the source code of a derived key; for code keys `dict_front::index` over the window's
    /// dictionary sizes, all ones when a code is at or past its dictionary's size. None (the hashed table) when the image
    /// carries no index or the end runs outside a site. Per row: one load a key and its compare, a multiply-add a key past
    /// the first; a pair's index is under the product of two `u32` sizes, so only three or more keys clamp their strides
    /// (at `dict_front::saturation`: a clamped stride times a code under its size stays under 2^47, so no sum wraps).
    fn index(&self, b: &mut FunctionBuilder<'_>, env: &Env, t: &TableDecl, img: &ImageWords) -> Option<Value> {
        t.key.dense?;
        let codes = match img.index.as_ref()? {
            FrontIndex::Source(c) => return Some(*c),
            FrontIndex::Codes(codes) => codes,
        };
        let s = env.site?;
        let lim = (codes.len() > 2).then(|| b.iconst(Type::I64, dict_front::saturation(crate::dict::codefront::cap(t)) as i64));
        let lanes = b.load(Type::Ptr, env.ctx, (site_off(s) + site::LANES) as i32);
        let (mut at, mut stride, mut off): (Option<Value>, Option<Value>, Option<Value>) = (None, None, None);
        for (i, &(c, l)) in codes.iter().enumerate() {
            let n = b.load_zext(Type::I32, lanes, l as i32 * LANE_SIZE as i32 + lane::NCODES_OR_SCALE as i32);
            let past = b.icmp(IntCond::Uge, c, n);
            off = Some(match off {
                Some(o) => b.int_bin(IntBinOp::Or, o, past),
                None => past,
            });
            at = Some(match (at, stride) {
                (Some(a), Some(s)) => {
                    let term = b.int_bin(IntBinOp::Mul, c, s);
                    b.int_bin(IntBinOp::Add, a, term)
                }
                _ => c,
            });
            if i + 1 < codes.len() {
                let wide = match stride {
                    Some(s) => b.int_bin(IntBinOp::Mul, s, n),
                    None => n,
                };
                stride = Some(match lim {
                    Some(lim) => {
                        let lo = b.icmp(IntCond::Ult, wide, lim);
                        b.select(lo, wide, lim)
                    }
                    None => wide,
                });
            }
        }
        let ones = b.iconst(Type::I64, -1);
        Some(b.select(off?, ones, at?))
    }
}

impl TableMech for Dense {
    /// Code keys the cost home takes by the planner's group estimate (`dict_front::takes`: a pair always, more keys when
    /// the estimate fits the capacity) or a key derived from one code lane (`KeyShape.dense`), no float slot, and never
    /// the one plain code key code.front_part takes.
    fn takes(&self, t: &TableDecl) -> bool {
        let float = (0..t.layout.nslots()).any(|j| matches!(t.layout.slot_kind(j), SlotKind::SumF64 | SlotKind::SumF32 | SlotKind::F64Accum));
        let keys = |d: DenseFront| d.n >= 2 && dict_front::takes(d.n, d.groups, crate::dict::codefront::cap(t));
        let take = t.key.dense.is_some_and(|d| d.derived || keys(d)) && t.key.code.is_none() && !float;
        if take {
            fires::fire("dict.front_dense"); // shapecheck fire signal (REGISTRY KEYED-10, B3-10)
        }
        take
    }

    fn state_bytes(&self, t: &TableDecl) -> u32 {
        FRONT.state_bytes(t)
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl) {
        FRONT.open_front(b, env, t)
    }

    fn upsert(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, h: Value) -> (Value, Value) {
        match self.index(b, env, t, img) {
            Some(i) => FRONT.upsert_at(b, env, t, img, i, false),
            None => crate::table::bypass::HASHED.upsert(b, env, t, img, h),
        }
    }

    fn find(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, h: Value) -> Value {
        match self.index(b, env, t, img) {
            Some(i) => FRONT.find_at(b, env, t, img, i),
            None => crate::table::bypass::HASHED.find(b, env, t, img, h),
        }
    }

    /// D3224 A4: the probe at the codes' index (code.front_part's `probe_at`).
    fn probe(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, codes: &ImageWords) -> Option<(gravity_ir::Block, gravity_ir::Block)> {
        let i = self.index(b, env, t, codes)?;
        Some(FRONT.probe_at(b, env, t, i, false))
    }

    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl) {
        FRONT.window_end(b, env, t)
    }

    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, x: &Exits) {
        FRONT.finish(b, env, t, x)
    }
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind};
    use gravity_abi::hash::Depth;

    use crate::table::bypass::HASHED;
    use crate::ops::agg::testkit::{fold, want, Case};
    use super::DENSE;
    use crate::cell::LaneId;
    use crate::table::face::{DenseFront, Domain, KeyShape, TableDecl, TableMech};

    fn small(i: i64) -> i64 {
        i % 60
    }
    fn big(i: i64) -> i64 {
        i % 10_000
    }
    fn small3(i: i64) -> i64 {
        small(i) / 3
    }
    fn big3(i: i64) -> i64 {
        big(i) / 3
    }

    /// The front against the hashed table over part-local codes (each window its own permutation), 15 windows, with and
    /// without NULL rows: a pair of code keys (60 values: dictionaries 7 x 9, all in the front; 10 000: 7 x 1429, past the
    /// capacity, the low tuples in the front and the rest in the table) and a key derived from a code (three codes per
    /// value, merged at the flush): per key the same rows and sums as the hashed table, and as the rows say.
    #[test]
    fn the_dense_front_folds_what_the_hashed_table_folds() {
        let n = 15 * 4096;
        for (key, derived, value) in [(small as fn(i64) -> i64, false, small as fn(i64) -> i64), (big, false, big), (small, true, small3), (big, true, big3)] {
            for null in [0, 11] {
                let case = |table: &'static dyn TableMech| Case { n, win: 4096, key, table, run: None, runs: None, code: true, null, dense: Some(derived) };
                let (front, _) = fold(case(&DENSE));
                let (table, _) = fold(case(&HASHED));
                assert_eq!(front, want(n, value, null), "front: derived {derived}, null {null}");
                assert_eq!(front, table, "front against the hashed table: derived {derived}, null {null}");
            }
        }
    }

    #[test]
    fn takes_code_keys_or_a_derived_key_and_leaves_one_plain_code_key() {
        let lane = (LaneId::col(1), 0);
        let t = |slot: SlotKind, code: bool, dense: Option<(u8, bool)>| {
            let layout = RecordLayout::with_sentinel(&[KeyKind::I64; 4], &[slot], false, None);
            let key = KeyShape { code: code.then_some(lane), dense: dense.map(|(n, derived)| DenseFront { n, derived, groups: 100 }), domain: Domain::Unknown };
            TableDecl { image_words: 4, record_bytes: 16, expect: None, depth: Depth::TOP, life: gravity_program::LifeId(0), layout, state_off: 0, key, facts: 0, copies: 0 }
        };
        assert!(DENSE.takes(&t(SlotKind::SumI64, false, Some((2, false)))), "a pair");
        assert!(DENSE.takes(&t(SlotKind::SumI64, false, Some((3, false)))), "three code keys estimated in the front");
        let past = |n: u8| {
            let mut x = t(SlotKind::SumI64, false, Some((n, false)));
            x.key.dense = x.key.dense.map(|d| DenseFront { groups: crate::dict::codefront::cap(&x) + 1, ..d });
            x
        };
        assert!(!DENSE.takes(&past(3)), "three code keys estimated past the front: the hashed table");
        assert!(DENSE.takes(&past(2)), "a pair at any estimate");
        assert!(DENSE.takes(&t(SlotKind::SumI64, false, Some((1, true)))), "a derived key");
        assert!(!DENSE.takes(&t(SlotKind::SumI64, true, None)), "one plain code key: code.front_part's");
        assert!(!DENSE.takes(&t(SlotKind::SumI64, false, None)), "no dense lanes");
        assert!(!DENSE.takes(&t(SlotKind::SumF64, false, Some((2, false)))), "a float slot");
    }
}
