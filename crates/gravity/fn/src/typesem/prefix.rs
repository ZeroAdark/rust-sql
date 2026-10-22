//! Order words of comparator-ordered keys whose PostgreSQL sort abbreviates them (and of a dictionary-coded bpchar's trimmed
//! render, `bytes_word`) (a sort key's inexact prefix word, lowerv4
//! rep/keylane.rs `pairs`): two values whose words differ order as the words do, compared as signed integers; equal words
//! are the comparator's to order (the run's var tail), so a compare calls it only on a tie. Each reads a varlena's payload
//! (after its header) and never panics on a payload PostgreSQL never writes (it reads a word, the tail still decides).

/// An inet / cidr payload's order word (network.c `network_abbrev_convert`, the 8-byte Datum arm, which adt_network's
/// `NetworkAbbrevState::convert` ports), its top bit flipped so the words order as signed integers: the family bit, then
/// for IPv4 the 32 netmasked address bits, the 6-bit netmask length and the top 25 subnet bits; for IPv6 the first 63
/// netmasked bits. `network_cmp` orders by family, then the address over the shorter netmask, then the netmask length,
/// then the whole address, which is the order of the words wherever they differ (PostgreSQL's own abbreviated compare).
pub fn net_word(payload: &[u8]) -> i64 {
    let (family, bits) = (payload.first().copied().unwrap_or(0), payload.get(1).copied().unwrap_or(0) as u32);
    let addr = payload.get(2..).unwrap_or(&[]);
    let mut a = [0u8; 8];
    let n = addr.len().min(8);
    a[..n].copy_from_slice(&addr[..n]);
    let abbrev = if family == adt_network::PGSQL_AF_INET6 {
        let ip = u64::from_be_bytes(a);
        let network = match bits {
            0 => 0,
            b if b < 64 => ip & !((1u64 << ((128 - b) % 64)) - 1),
            _ => ip,
        };
        1 << 63 | network >> 1
    } else {
        // IPv4: (32 - bits) subnet bits (all 32 at /0, whose network part is 0), the top 25 of them kept
        let ip = u32::from_be_bytes([a[0], a[1], a[2], a[3]]) as u64;
        let size = 32u32.saturating_sub(bits);
        let (mask, network) = if bits == 0 { (u64::MAX, 0) } else { ((1u64 << size) - 1, ip & !((1u64 << size) - 1)) };
        network << 31 | (bits as u64) << 25 | (ip & mask) >> size.saturating_sub(25)
    };
    (abbrev ^ 1 << 63) as i64
}

/// The data bytes a `bits_word` holds ahead of its length byte: a bit string of at most `8 * BITS_WORD_BYTES` bits is its
/// whole word.
pub const BITS_WORD_BYTES: usize = 7;

/// A bit / varbit payload's order word (`bit_cmp`: the data bytes by memcmp over the shorter, then the bit length): its
/// first 7 data bytes (past the 4-byte bit length) big-endian and zero-padded, top bit flipped, then one length byte: twice
/// the bit length for a value of at most 56 bits (`BITS_WORD_BYTES`) whose data is exactly its bytes (its low bit clear: the word
/// is the whole value, so equal words are equal values), 0xff for a longer one (low bit set: equal words are the
/// comparator's). A shorter value's zero padding never orders it above a longer one that `bit_cmp` puts after it (equal
/// over its bytes, it is the shorter, and its length byte is the lower: a long value's 0xff is above every short one's).
pub fn bits_word(payload: &[u8]) -> i64 {
    let data = payload.get(4..).unwrap_or(&[]);
    let bitlen = payload.get(..4).map_or(-1, |l| i32::from_ne_bytes([l[0], l[1], l[2], l[3]]));
    let mut w = [0u8; 8];
    let n = data.len().min(BITS_WORD_BYTES);
    w[..n].copy_from_slice(&data[..n]);
    let exact = (0..=8 * BITS_WORD_BYTES as i32).contains(&bitlen) && data.len() == (bitlen as usize).div_ceil(8);
    w[BITS_WORD_BYTES] = if exact { 2 * bitlen as u8 } else { 0xff };
    (u64::from_be_bytes(w) ^ 1 << 63) as i64
}

/// Whether a `bits_word` is the whole value (its low bit clear): two equal such words are equal bit strings.
pub const fn bits_word_exact(w: i64) -> bool {
    w & 1 == 0
}

/// A byte string's order word under a bytes-ordered collation (a bpchar payload with its trailing blanks trimmed, as
/// lowerv4's varlena `payload` hands it): its first 8 bytes big-endian and zero-padded, top bit flipped. A shorter string
/// equal over its bytes orders first, as memcmp-then-length does.
pub fn bytes_word(payload: &[u8]) -> i64 {
    let mut w = [0u8; 8];
    let n = payload.len().min(8);
    w[..n].copy_from_slice(&payload[..n]);
    (u64::from_be_bytes(w) ^ 1 << 63) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use adt_network::{InetRef, PGSQL_AF_INET6};

    fn inet(s: &str, cidr: bool) -> Vec<u8> {
        let v = adt_network::network_in(s, cidr, None).unwrap().unwrap();
        let mut p = vec![v.family, v.bits]; // alloc: Program
        p.extend_from_slice(&v.ipaddr[..if v.family == PGSQL_AF_INET6 { 16 } else { 4 }]);
        p
    }

    /// Every pair of 40 inet / cidr values: the word is PostgreSQL's abbreviation (adt_network's port, top bit flipped),
    /// equal values share one, and a lower word is a lower value (`network_cmp_internal`).
    #[test]
    fn net_words_are_pg_abbreviations_ordered_as_network_cmp() {
        let lits = [("0.0.0.0/0", false), ("0.0.0.1/0", false), ("0.0.0.2/0", false), ("255.255.255.255/0", false), ("10.0.0.0/8", true),
            ("10.1.0.0/16", true), ("10.1.2.3", false), ("10.1.2.3/8", false), ("10.1.2.3/24", false), ("10.1.2.4/24", false), ("10.2.0.0/16", true),
            ("9.0.0.0/16", true), ("10.0.0.0/7", false), ("192.168.1.1", false), ("192.168.1.1/31", false), ("192.168.1.0/31", false),
            ("192.168.1.128/25", true), ("192.168.1.255", false), ("128.0.0.0/1", true), ("127.255.255.255", false), ("1.2.3.4/32", false),
            ("::/0", false), ("::1", false), ("::2/0", false), ("ffff::/16", true), ("ffff::1/16", false), ("fe80::/10", true),
            ("fe80::1/64", false), ("fe80::1/63", false), ("fe80::2/128", false), ("2001:db8::/32", true), ("2001:db8::1", false),
            ("2001:db8:0:0:1::/80", true), ("2001:db8:0:0:1::1/80", false), ("2001:db8::ffff:ffff:ffff:fffe", false),
            ("2001:db8::ffff:ffff:ffff:ffff", false), ("::ffff:10.1.2.3", false), ("8000::/1", true), ("7fff:ffff:ffff:ffff::/64", true),
            ("7fff:ffff:ffff:fffe::/63", true)];
        let ps: Vec<Vec<u8>> = lits.iter().map(|&(s, c)| inet(s, c)).collect(); // alloc: Program
        for i in 0..ps.len() {
            let pg = adt_network::abbrev::NetworkAbbrevState::new().convert(InetRef::from_payload(&ps[i]));
            assert_eq!(net_word(&ps[i]), (pg ^ 1 << 63) as i64, "{}", lits[i].0);
            for j in 0..ps.len() {
                let c = adt_network::network_cmp_internal(InetRef::from_payload(&ps[i]), InetRef::from_payload(&ps[j]));
                let (wi, wj) = (net_word(&ps[i]), net_word(&ps[j]));
                if c == 0 {
                    assert_eq!(wi, wj, "{} = {}", lits[i].0, lits[j].0);
                }
                if wi < wj {
                    assert!(c < 0, "{} word below {} but not the value", lits[i].0, lits[j].0);
                }
            }
        }
        assert_eq!(net_word(&ps[1]), net_word(&ps[2]), "past 25 subnet bits a tie is the comparator's");
    }

    /// Byte strings in memcmp-then-length order have ascending or equal words (equal only past 8 bytes).
    #[test]
    fn bytes_words_order_as_memcmp() {
        let vals: [&[u8]; 9] = [b"", b"\x01", b"A", b"AB", b"AB\t", b"ABC", b"Q Q", b"X", b"\xff"];
        assert!(vals.windows(2).all(|w| bytes_word(w[0]) < bytes_word(w[1])), "ascending");
        assert_eq!(bytes_word(b"abcdefghX"), bytes_word(b"abcdefghY"), "past 8 bytes the comparator decides");
    }

    fn bits(n: i32, data: &[u8]) -> Vec<u8> {
        let mut p = n.to_ne_bytes().to_vec(); // alloc: Program
        p.extend_from_slice(data);
        p
    }

    /// Every pair of 29 bit strings: equal values share a word, a lower word is a lower value (`bit_cmp_payload`), and two
    /// equal exact words (`bits_word_exact`: at most 56 bits) are equal values.
    #[test]
    fn bits_words_order_as_bit_cmp() {
        let vals = [bits(0, &[]), bits(1, &[0x00]), bits(1, &[0x80]), bits(2, &[0x80]), bits(8, &[0x80]), bits(9, &[0x80, 0x00]),
            bits(16, &[0x80, 0x01]), bits(8, &[0x7f]), bits(16, &[0x7f, 0xff]), bits(64, &[1, 2, 3, 4, 5, 6, 7, 8]),
            bits(65, &[1, 2, 3, 4, 5, 6, 7, 8, 0x00]), bits(72, &[1, 2, 3, 4, 5, 6, 7, 8, 0xff]), bits(72, &[1, 2, 3, 4, 5, 6, 7, 9, 0x00]),
            bits(6, &[0xfc]), bits(6, &[0x04]), bits(3, &[0xe0]), bits(7, &[0xfe]), bits(55, &[1, 2, 3, 4, 5, 6, 6]),
            bits(56, &[1, 2, 3, 4, 5, 6, 7]), bits(56, &[1, 2, 3, 4, 5, 6, 8]), bits(57, &[1, 2, 3, 4, 5, 6, 7, 0x00]),
            bits(57, &[1, 2, 3, 4, 5, 6, 7, 0x80]), bits(64, &[1, 2, 3, 4, 5, 6, 7, 0xff]), bits(56, &[0xff; 7]),
            bits(57, &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x80]), bits(48, &[1, 2, 3, 4, 5, 6]), bits(49, &[1, 2, 3, 4, 5, 6, 0]),
            bits(24, &[0, 0, 0]), bits(25, &[0, 0, 0, 0])];
        for a in &vals {
            for b in &vals {
                let c = adt_varbit::bit_cmp_payload(a, b);
                let (wa, wb) = (bits_word(a), bits_word(b));
                if c == 0 {
                    assert_eq!(wa, wb, "{a:?} = {b:?}");
                }
                if wa < wb {
                    assert!(c < 0, "{a:?} word below {b:?} but not the value");
                }
                if wa == wb && bits_word_exact(wa) {
                    assert_eq!(c, 0, "{a:?} and {b:?} share an exact word");
                }
            }
        }
        let exact: Vec<bool> = vals.iter().map(|v| bits_word_exact(bits_word(v))).collect(); // alloc: Program
        assert!(vals.iter().zip(&exact).all(|(v, &e)| e == (i32::from_ne_bytes([v[0], v[1], v[2], v[3]]) <= 8 * BITS_WORD_BYTES as i32)), "exact up to 56 bits");
        assert!(!bits_word_exact(bits_word(&bits(8, &[0x80, 0x00]))), "a data length that is not the bit length's is never exact");
        assert!(!bits_word_exact(bits_word(&[1, 2])), "a truncated payload is never exact");
    }
}
