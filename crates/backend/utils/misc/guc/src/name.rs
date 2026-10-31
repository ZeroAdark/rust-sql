use core::cmp::Ordering;

pub const MAP_OLD_GUC_NAMES: &[(&str, &str)] = &[
    ("sort_mem", "work_mem"),
    ("vacuum_mem", "maintenance_work_mem"),
    ("ssl_ecdh_curve", "ssl_groups"),
];

// ASCII-only downcasing, deliberately not strcasecmp: stable across setlocale().
pub fn guc_name_compare(namea: &str, nameb: &str) -> Ordering {
    let a = namea.as_bytes();
    let b = nameb.as_bytes();
    let n = a.len().min(b.len());
    for i in 0..n {
        let cha = a[i].to_ascii_lowercase();
        let chb = b[i].to_ascii_lowercase();
        if cha != chb {
            return cha.cmp(&chb);
        }
    }
    a.len().cmp(&b.len())
}

#[inline]
pub fn guc_name_eq(namea: &str, nameb: &str) -> bool {
    guc_name_compare(namea, nameb) == Ordering::Equal
}

pub fn guc_name_hash(name: &str) -> u32 {
    let mut result: u32 = 0;
    for &b in name.as_bytes() {
        result = result.rotate_left(5);
        result ^= b.to_ascii_lowercase() as u32;
    }
    result
}

pub fn fold_name(name: &str) -> String {
    // ASCII-only case fold, preserving non-ASCII bytes exactly (parity with C's
    // per-byte tolower). The old `name.bytes().map(|b| b as char)` reinterpreted
    // each >=0x80 continuation byte as a U+0080..U+00FF code point and re-encoded
    // it, so the folded insert key diverged from find_index's raw lookup for
    // legal non-ASCII custom GUC names — every SET leaked a fresh placeholder.
    // ASCII case folding preserves the byte length, so allocate once instead of
    // growing a String from the character iterator's conservative size hint.
    name.to_ascii_lowercase()
}

pub fn convert_guc_name_for_parameter_acl(name: &str) -> String {
    let mut canonical = name;
    for (old, new) in MAP_OLD_GUC_NAMES {
        if guc_name_eq(name, old) {
            canonical = new;
            break;
        }
    }
    fold_name(canonical)
}

// guc_name_hash-compatible BuildHasher over pre-folded keys (the guc_hashtab
// hash/match pair).
#[derive(Clone, Copy, Default)]
pub struct GucNameHasherBuilder;

pub struct GucNameHasher(u32);

impl std::hash::Hasher for GucNameHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if b == 0xff {
                continue;
            }
            self.0 = self.0.rotate_left(5);
            self.0 ^= b.to_ascii_lowercase() as u32;
        }
    }
    fn finish(&self) -> u64 {
        // The table takes a bucket from the hash's low bits and each slot's tag from its top 7: the u32 zero-extended
        // gave every name tag 0, so a lookup or insert compared its key against every full slot of the probed group. A
        // multiply by an odd constant spreads all 32 bits into the top ones and keeps the low bits a bijection of the
        // u32's low bits (the same buckets as before).
        (self.0 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
    }
}

impl std::hash::BuildHasher for GucNameHasherBuilder {
    type Hasher = GucNameHasher;
    fn build_hasher(&self) -> GucNameHasher {
        GucNameHasher(0)
    }
}

// hash_seq_search order over guc_hashtab (guc.c:974, dynahash): buckets
// ascending, chains in insertion order. C's 18.6 core tables size it at
// 512 buckets and never split it; pinned rather than derived from len(),
// which also counts the pgrust-only variables.
pub const C_GUC_HASH_NBUCKETS: u32 = 512;

pub fn hash_seq_order(reg: &crate::registry::GucRegistry) -> Vec<usize> {
    let mut keyed: Vec<(u32, usize)> = Vec::new();
    let _ = keyed.try_reserve(reg.len());
    for idx in 0..reg.len() {
        let gen = reg[idx].gen();
        if gen.status & crate::model::GUC_REMOVED != 0 {
            continue;
        }
        keyed.push((guc_name_hash(gen.name.as_ref()) & (C_GUC_HASH_NBUCKETS - 1), idx));
    }
    keyed.sort_unstable();
    let mut out = Vec::new();
    let _ = out.try_reserve(keyed.len());
    for (_, idx) in keyed {
        out.push(idx);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::hash::BuildHasher;

    use super::GucNameHasherBuilder;

    #[test]
    fn the_names_tags_spread_over_the_top_bits() {
        let names: Vec<String> = guc_tables::all_settings().map(|s| super::fold_name(s.name())).collect();
        assert!(names.len() > 300, "the built-ins: {}", names.len());
        let mut tags = [0u32; 128];
        for n in &names {
            let h = GucNameHasherBuilder.hash_one(n.as_str());
            tags[(h >> 57) as usize] += 1;
            assert_eq!(h, GucNameHasherBuilder.hash_one(n.to_ascii_uppercase().as_str()), "{n}: case-blind");
        }
        let used = tags.iter().filter(|&&c| c != 0).count();
        // a uniform spread of ~450 names over 128 tags uses ~120; the zero-extended u32 used 1
        assert!(used > 100, "{used} of 128 tags used");
    }
}
