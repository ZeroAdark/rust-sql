//! `fp`: the SipHash-2-4-128 reference vectors, the process seed, the `Fp128` table.
mod common;
use common::{Buf, VecDict};
use gravity_dict::fp::{canon_inline, canon_of, fp128, install, keys, seed, siphash128, HASHED, INLINE_BYTES};
use gravity_dict::{BuildError, Fp128, View};

/// `vectors_sip128` of the reference implementation (veorq/SipHash): key 00..0f, input 0..len-1.
const VECTORS_SIP128: [&str; 64] = [
    "a3817f04ba25a8e66df67214c7550293", "da87c1d86b99af44347659119b22fc45", "8177228da4a45dc7fca38bdef60affe4", "9c70b60c5267a94e5f33b6b02985ed51",
    "f88164c12d9c8faf7d0f6e7c7bcd5579", "1368875980776f8854527a07690e9627", "14eeca338b208613485ea0308fd7a15e", "a1f1ebbed8dbc153c0b84aa61ff08239",
    "3b62a9ba6258f5610f83e264f31497b4", "264499060ad9baabc47f8b02bb6d71ed", "00110dc378146956c95447d3f3d0fbba", "0151c568386b6677a2b4dc6f81e5dc18",
    "d626b266905ef35882634df68532c125", "9869e247e9c08b10d029934fc4b952f7", "31fcefac66d7de9c7ec7485fe4494902", "5493e99933b0a8117e08ec0f97cfc3d9",
    "6ee2a4ca67b054bbfd3315bf85230577", "473d06e8738db89854c066c47ae47740", "a426e5e423bf4885294da481feaef723", "78017731cf65fab074d5208952512eb1",
    "9e25fc833f2290733e9344a5e83839eb", "568e495abe525a218a2214cd3e071d12", "4a29b54552d16b9a469c10528eff0aae", "c9d184ddd5a9f5e0cf8ce29a9abf691c",
    "2db479ae78bd50d8882a8a178a6132ad", "8ece5f042d5e447b5051b9eacb8d8f6f", "9c0b53b4b3c307e87eaee08678141f66", "abf248af69a6eae4bfd3eb2f129eeb94",
    "0664da1668574b88b935f3027358aef4", "aa4b9dc4bf337de90cd4fd3c467c6ab7", "ea5c7f471faf6bde2b1ad7d4686d2287", "2939b0183223fafc1723de4f52c43d35",
    "7c3956ca5eeafc3e363e9d556546eb68", "77c6077146f01c32b6b69d5f4ea9ffcf", "37a6986cb8847edf0925f0f1309b54de", "a705f0e69da9a8f907241a2e923c8cc8",
    "3dc47d1f29c448461e9e76ed904f6711", "0d62bf01e6fc0e1a0d3c4751c5d3692b", "8c03468bca7c669ee4fd5e084bbee7b5", "528a5bb93baf2c9c4473cce5d0d22bd9",
    "df6a301e95c95dad97ae0cc8c6913bd8", "801189902c857f39e73591285e70b6db", "e617346ac9c231bb3650ae34ccca0c5b", "27d93437efb721aa401821dcec5adf89",
    "89237d9ded9c5e78d8b1c9b166cc7342", "4a6d8091bf5e7d651189fa94a250b14c", "0e33f96055e7ae893ffc0e3dcf492902", "e61c432b720b19d18ec8d84bdc63151b",
    "f7e5aef549f782cf379055a608269b16", "438d030fd0b7a54fa837f2ad201a6403", "a590d3ee4fbf04e3247e0d27f286423f", "5fe2c1a172fe93c4b15cd37caef9f538",
    "2c97325cbd06b36eb2133dd08b3a017c", "92c814227a6bca949ff0659f002ad39e", "dce850110bd8328cfbd50841d6911d87", "67f14984c7da791248e32bb5922583da",
    "1938f2cf72d54ee97e94166fa91d2a36", "74481e9646ed49fe0f6224301604698e", "57fca5de98a9d6d8006438d0583d8a1d", "9fecde1cefdc1cbed4763674d9575359",
    "e3040c00eb28f15366ca73cbd872e740", "7697009a6a831dfecca91c5993670f7a", "5853542321f567a005d547a4f04759bd", "5150d1772f50834a503e069a973fbd7c",
];

fn unhex16(s: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

#[test]
fn siphash128_matches_the_reference_vectors() {
    let k0 = u64::from_le_bytes([0, 1, 2, 3, 4, 5, 6, 7]);
    let k1 = u64::from_le_bytes([8, 9, 10, 11, 12, 13, 14, 15]);
    let input: Vec<u8> = (0u8..64).collect();
    for (len, want) in VECTORS_SIP128.iter().enumerate() {
        let [lo, hi] = siphash128(k0, k1, &input[..len]);
        let mut got = [0u8; 16];
        got[..8].copy_from_slice(&lo.to_le_bytes());
        got[8..].copy_from_slice(&hi.to_le_bytes());
        assert_eq!(got, unhex16(want), "vectors_sip128[{len}]");
    }
}

#[test]
fn the_seed_installs_once_and_the_table_carries_the_seeded_fingerprints() {
    let d = VecDict::of(&["", "a", "b", "hello", "http://example.com/some/url?q=1"]);
    let mut buf = Buf::new(4096);
    let mut r = buf.region();
    if seed().is_none() {
        assert_eq!(fp128(b""), None);
        assert_eq!(Fp128::build(&d, &mut r, &mut |v| Ok(v)).err(), Some(BuildError::NoSeed));
    }
    install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
    assert!(!install(1, 2), "the first seed stays");
    let (k0, k1) = seed().unwrap();
    let t = Fp128::build(&d, &mut r, &mut |v| Ok(v)).unwrap();
    for (i, e) in d.entries.iter().enumerate() {
        assert_eq!(unsafe { t.get(i as u32) }, siphash128(k0, k1, e), "code {i}");
        assert_eq!(fp128(e), Some(unsafe { t.get(i as u32) }));
    }
    assert_ne!(unsafe { t.get(1) }, unsafe { t.get(2) });
    let lower = Fp128::build(&d, &mut r, &mut |v| Ok(View::of(if unsafe { v.bytes() } == b"hello" { b"a" } else { unsafe { v.bytes() } }))).unwrap();
    assert_eq!(unsafe { lower.get(d.code_of("hello")) }, unsafe { lower.get(d.code_of("a")) });
    assert!(!t.has_faults());
    assert_eq!(t.ncodes(), 5);
}

/// textkeys step 3 (road key.canon): the canonical image is a payload's own bytes and length up to `INLINE_BYTES` (bit 63
/// clear), else its Fp128 with bit 63 set; equal payloads equal images, the two forms never meet; a dictionary's image
/// table (`Fp128::build_canon`) holds each entry's, the same with or without the part's Fp128 table to derive from.
#[test]
fn the_canonical_image_is_the_bytes_up_to_15_else_the_fingerprint_under_one_bit() {
    install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
    let k = keys().unwrap();
    assert_eq!(canon_inline(b""), [0, 0], "the empty string");
    assert_eq!(canon_inline(b"ab"), [0x6261, 2 << 56]);
    assert_eq!(canon_inline(b"\0"), [0, 1 << 56], "a zero byte is not the empty string");
    assert_eq!(canon_inline(b"abcdefghi"), [u64::from_le_bytes(*b"abcdefgh"), b'i' as u64 | 9 << 56]);
    let fifteen = b"abcdefghijklmno";
    assert_eq!(fifteen.len(), INLINE_BYTES);
    assert_eq!(canon_inline(fifteen)[1] >> 56, 15, "15 bytes: inline, its length in bits 56..60");
    assert_eq!(k.canon_bytes(fifteen), canon_inline(fifteen));
    let sixteen = b"abcdefghijklmnop";
    let fp = k.fp_bytes(sixteen);
    assert_eq!(k.canon_bytes(sixteen), [fp[0], fp[1] | HASHED], "16 bytes: the fingerprint under the flag bit");
    assert_eq!(canon_of(sixteen, || fp), k.canon_bytes(sixteen));
    for n in 0..=INLINE_BYTES {
        let b = &[0xFFu8; 15][..n];
        assert_eq!(k.canon_bytes(b)[1] & HASHED, 0, "{n} bytes: the flag bit clear");
    }
    assert_ne!(k.canon_bytes("Ä-Öü €".as_bytes()), k.canon_bytes("Ä-Öü  ".as_bytes()), "multibyte bytes compare as bytes");
    let d = VecDict::of(&["", "a", "abcdefghijklmno", "abcdefghijklmnop", "http://example.com/some/url?q=1", "Ä-Öü €"]);
    let mut buf = Buf::new(8192);
    let mut r = buf.region();
    let raw = Fp128::build(&d, &mut r, &mut |v| Ok(v)).unwrap();
    let derived = Fp128::build_canon(&d, &mut r, &mut |v| Ok(v), Some(&raw)).unwrap();
    let hashed = Fp128::build_canon(&d, &mut r, &mut |v| Ok(v), None).unwrap();
    for (i, e) in d.entries.iter().enumerate() {
        let c = i as u32;
        assert_eq!(unsafe { derived.get(c) }, k.canon_bytes(e), "code {i}: derived from the Fp128 table");
        assert_eq!(unsafe { hashed.get(c) }, k.canon_bytes(e), "code {i}: hashed in the build");
    }
    assert_eq!(derived.ncodes(), 6);
    assert!(!derived.has_faults());
}
