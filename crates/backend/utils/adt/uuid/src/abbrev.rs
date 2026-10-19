//! uuid_abbrev_convert/uuid_abbrev_abort.

use crate::PgUuid;
use hyperloglog::HyperLogLog;

pub struct UuidAbbrevState {
    input_count: i64,
    estimating: bool,
    abbr_card: HyperLogLog,
}

impl UuidAbbrevState {
    pub fn new() -> UuidAbbrevState {
        UuidAbbrevState {
            input_count: 0,
            estimating: true,
            abbr_card: HyperLogLog::new(10),
        }
    }

    /// `uuid_abbrev_convert`; returns the native-endian abbreviated key word.
    pub fn convert(&mut self, uuid: &PgUuid) -> u64 {
        let mut prefix = [0u8; 8];
        prefix.copy_from_slice(&uuid[..8]);
        self.input_count += 1;
        if self.estimating {
            let res = u64::from_ne_bytes(prefix);
            self.abbr_card
                .add(hashfn::hash_bytes_uint32(res as u32 ^ (res >> 32) as u32));
        }
        u64::from_be_bytes(prefix)
    }

    /// `uuid_abbrev_abort`: commit once past 100k distinct abbrevs, abort
    /// below 1 distinct per ~2k non-null inputs.
    pub fn abort(&mut self, memtupcount: i32) -> bool {
        if memtupcount < 10000 || self.input_count < 10000 || !self.estimating {
            return false;
        }
        let card = self.abbr_card.estimate();
        if card > 100000.0 {
            if trace_sort() {
                trace_log(estimation_ends_line(card, self.input_count, memtupcount));
            }
            self.estimating = false;
            return false;
        }
        let threshold = self.input_count as f64 / 2000.0 + 0.5;
        if card < threshold {
            if trace_sort() {
                trace_log(aborting_line(card, threshold, self.input_count, memtupcount));
            }
            return true;
        }
        if trace_sort() {
            trace_log(cardinality_line(card, self.input_count, memtupcount));
        }
        false
    }
}

fn estimation_ends_line(card: f64, input_count: i64, memtupcount: i32) -> String {
    format!(
        "uuid_abbrev: estimation ends at cardinality {card:.6} after {input_count} values \
         ({memtupcount} rows)"
    )
}

fn aborting_line(card: f64, threshold: f64, input_count: i64, memtupcount: i32) -> String {
    format!(
        "uuid_abbrev: aborting abbreviation at cardinality {card:.6} below threshold \
         {threshold:.6} after {input_count} values ({memtupcount} rows)"
    )
}

fn cardinality_line(card: f64, input_count: i64, memtupcount: i32) -> String {
    format!("uuid_abbrev: cardinality {card:.6} after {input_count} values ({memtupcount} rows)")
}

fn trace_sort() -> bool {
    guc_tables::vars::trace_sort.installed() && guc_tables::vars::trace_sort.read()
}

#[cold]
#[inline(never)]
fn trace_log(message: String) {
    let _ = elog::ereport(types_error::LOG)
        .errmsg_internal(message)
        .finish(types_error::ErrorLocation::new(file!(), line!() as i32, "uuid_abbrev_abort"));
}

impl Default for UuidAbbrevState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbrev_orders_like_uuid_internal_cmp() {
        let mut st = UuidAbbrevState::new();
        let mut x: u64 = 42;
        let mut uuids: Vec<PgUuid> = Vec::new();
        for i in 0..200u64 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let mut u = [0u8; 16];
            u[..8].copy_from_slice(&(x >> (i % 5)).to_be_bytes());
            u[8..].copy_from_slice(&x.to_le_bytes());
            uuids.push(u);
        }
        let abbrevs: Vec<u64> = uuids.iter().map(|u| st.convert(u)).collect();
        for i in 0..uuids.len() {
            for j in 0..uuids.len() {
                let a = (abbrevs[i] > abbrevs[j]) as i32 - (abbrevs[i] < abbrevs[j]) as i32;
                if a != 0 {
                    assert_eq!(a, crate::uuid_internal_cmp(&uuids[i], &uuids[j]).signum());
                }
            }
        }
    }

    #[test]
    fn abort_gates() {
        let mut st = UuidAbbrevState::new();
        let u = [1u8; 16];
        st.convert(&u);
        assert!(!st.abort(9999));

        // 20k copies of one value: cardinality 1 < 20000/2000 + 0.5.
        let mut st = UuidAbbrevState::new();
        for _ in 0..20000 {
            st.convert(&u);
        }
        assert!(st.abort(20000));
    }

    // uuid.c:363-393 trace_sort lines, %f = six decimals, INT64_FORMAT / %d.
    #[test]
    fn trace_sort_lines_match_c() {
        assert_eq!(
            cardinality_line(10259.653083, 10240, 10240),
            "uuid_abbrev: cardinality 10259.653083 after 10240 values (10240 rows)"
        );
        assert_eq!(
            aborting_line(1.000489, 5.62, 10240, 10240),
            "uuid_abbrev: aborting abbreviation at cardinality 1.000489 below threshold \
             5.620000 after 10240 values (10240 rows)"
        );
        assert_eq!(
            estimation_ends_line(100000.5, 200000, 200000),
            "uuid_abbrev: estimation ends at cardinality 100000.500000 after 200000 values \
             (200000 rows)"
        );
    }
}
