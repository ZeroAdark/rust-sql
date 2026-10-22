//! The whole-payload LIKE build (`Matcher::build_bits_over`: the rare-byte-pair scan over one
//! payload, hits resolved to entries) against the per-entry build and `adt_like` over a synthetic
//! part payload (4B-U images back to back, as pgrc lays a dictionary out), every class, negated
//! too; and `substr::RarePair` against a naive search over random haystacks with needles whose
//! rare bytes repeat and straddle entry boundaries; the pair re-ranked from the data's own byte
//! counts (B2-01) finds the same occurrences. D3288: a Segments pattern streams too (its first run
//! scanned, a hit's entry decided whole by its runs), equal to the per-entry build.
mod common;

use common::{url_corpus, Buf};
use gravity_dict::like::EntryIndex;
use gravity_dict::substr::RarePair;
use gravity_dict::{Bits, Matcher, Mode};

struct Payload {
    bytes: Vec<u8>,
    spans: Vec<(usize, usize)>,
}

impl Payload {
    fn of(entries: &[Vec<u8>]) -> Payload {
        let (mut bytes, mut spans) = (Vec::new(), Vec::new());
        for e in entries {
            bytes.extend_from_slice(&(((e.len() + 4) as u32) << 2).to_le_bytes());
            spans.push((bytes.len(), e.len()));
            bytes.extend_from_slice(e);
        }
        Payload { bytes, spans }
    }
}

impl EntryIndex for Payload {
    fn ncodes(&self) -> u32 {
        self.spans.len() as u32
    }

    fn span(&self, code: u32) -> (usize, usize) {
        self.spans[code as usize]
    }
}

fn bits_of(b: &Bits) -> Vec<bool> {
    (0..b.ncodes()).map(|c| unsafe { b.get(c) }).collect()
}

#[test]
fn the_payload_build_equals_the_per_entry_build_and_like_match_c() {
    let mut entries = url_corpus(5000, 3);
    for extra in ["", "g", "gg", "garden", "garde", "arden", "%garden%", "xgardenx", "gardengarden", "gar\u{0}den", "日本garden", "GARDEN", "Garden"] {
        entries.push(extra.as_bytes().to_vec());
    }
    entries.sort();
    entries.dedup();
    let payload = Payload::of(&entries);
    let dict = common::VecDict { entries: entries.clone(), null: None };
    let mut buf = Buf::new(1 << 20);
    let patterns = ["%garden%", "%Garden%", "%.garden.%", "%e%", "%%", "%", "", "%日本%", "%.рф%", "http://%", "%/", "http://example.com/", "%ar%", "%en%en%", "%gar_en%", "%\\%%", "%post-1%", "%/index.html", "%a.b.c.d%", "%rde%"];
    let mut streamed = 0;
    for p in patterns {
        for negated in [false, true] {
            let mut r = buf.region();
            let mut m = Matcher::new(p.as_bytes(), Mode::Like, &mut r).unwrap();
            if negated {
                m = m.negated();
            }
            let over = m.build_bits_over(&payload, &payload.bytes, &mut r).unwrap();
            let per = m.build_bits(&dict, &mut r).unwrap();
            let (got, want) = (bits_of(&over), bits_of(&per));
            for (c, e) in entries.iter().enumerate() {
                let oracle = adt_like::utf8_match_text(e, p.as_bytes(), None).unwrap() == adt_like::LIKE_TRUE;
                assert_eq!(got[c], oracle != negated, "{p:?} negated={negated} on {:?}", String::from_utf8_lossy(e));
            }
            assert_eq!(got, want, "{p:?} negated={negated}");
            if m.class() == gravity_dict::like::Class::Contains {
                streamed += 1;
            }
        }
    }
    println!("{} patterns, {streamed} contains builds streamed over a {} B payload of {} entries", patterns.len(), payload.bytes.len(), entries.len());
    assert!(streamed >= 20);
}

#[test]
fn the_rare_pair_scan_finds_every_first_occurrence() {
    let mut x = 0x9E37_79B9u64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let alphabet = b"aaaaabbbcgg.//e\x80\xC3\xA9";
    for needle_len in 2..=9usize {
        for _ in 0..40 {
            let needle: Vec<u8> = (0..needle_len).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect();
            let pair = RarePair::new(&needle).unwrap();
            for hay_len in [0usize, 1, 5, 15, 16, 17, 31, 32, 33, 47, 48, 64, 100, 257] {
                let mut hay: Vec<u8> = (0..hay_len).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect();
                if hay_len > needle_len && next() % 2 == 0 {
                    let at = (next() % (hay_len - needle_len) as u64) as usize;
                    hay[at..at + needle_len].copy_from_slice(&needle);
                }
                for from in [0usize, 1, 3, hay_len / 2, hay_len] {
                    let want = (from..=hay_len.saturating_sub(needle_len)).find(|&i| i + needle_len <= hay_len && hay[i..i + needle_len] == needle[..]);
                    let want = if from + needle_len > hay_len { None } else { want };
                    assert_eq!(pair.find(&needle, &hay, from), want, "needle {needle:?} hay {hay:?} from {from}");
                }
            }
        }
    }
    let (o1, o2) = RarePair::new(b"garden").unwrap().offsets();
    assert!(o1 != o2 && b"garden"[o1] == b'g', "garden's rare pair {o1} {o2}");
}

/// Item 18 (B2-01): the payload scan's pair is re-ranked from the payload's own byte counts. Over data where the
/// English-rare letters are the common ones, `RarePair::new` picks them; `RarePair::sampled` picks the needle's bytes the
/// data rarely holds, a byte the counts never saw rarest, the static rank breaking a tie. `find_rechecking` stops at the
/// first block where the rule fires with nothing found before it, and resuming there finds what `find` finds.
#[test]
fn the_rechecked_pair_takes_the_payloads_rare_bytes() {
    use gravity_dict::substr::{count_bytes, Step};
    let hay: Vec<u8> = (0..(1u32 << 16)).map(|i| b"qzqxzjqzxqjzu"[(i as usize * 7) % 13]).collect();
    let mut counts = [0u64; 256];
    count_bytes(&hay, &mut counts);
    assert_eq!(counts.iter().sum::<u64>(), hay.len() as u64);
    let needle = b"quiz";
    let (s1, s2) = RarePair::new(needle).unwrap().offsets();
    let (d1, d2) = RarePair::sampled(needle, &counts).unwrap().offsets();
    let pick = |a: usize, b: usize| {
        let mut v = [needle[a], needle[b]];
        v.sort();
        v
    };
    assert_eq!(pick(s1, s2), *b"qz", "the static rank: q and z are rare in English");
    assert_eq!(pick(d1, d2), *b"iu", "the counts: i never occurs, u is the rarest of the rest");
    let mut hay2 = hay.clone();
    hay2.extend_from_slice(b"..quiz..quiz");
    let (a, b) = (RarePair::new(needle).unwrap(), RarePair::sampled(needle, &counts).unwrap());
    let near: Vec<u8> = b"qzqz quz quiq qqzz ".iter().cycle().take(4096).copied().chain(*b"..quiz..").collect();
    for h in [&hay2, &near] {
        for from in [0usize, 7, h.len() / 2, h.len() - 8, h.len() - 4] {
            let want = a.find(needle, h, from);
            assert_eq!(b.find(needle, h, from), want, "from {from}");
            let mut rejected = 0u64;
            let got = match a.find_rechecking(needle, h, from, &mut rejected, |_, r| r >= 3) {
                Step::Hit(j) => Some(j),
                Step::End => None,
                Step::Recheck(at) => {
                    assert!(rejected >= 3 && at > from && want.map_or(true, |w| w >= at), "recheck at {at} from {from} want {want:?}");
                    b.find(needle, h, at)
                }
            };
            assert_eq!(got, want, "rechecking from {from}");
        }
    }
}

/// The payload build and the per-entry build under a re-check rule (re-rank at the first rejected candidate, at every
/// wasted block, and never) equal each other and like_match.c, over entries whose common letters are the English-rare
/// ones.
#[test]
fn the_recounting_payload_build_equals_the_per_entry_build() {
    let mut x = 0x2545_F491u64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let letters = b"qzxjqzxjkvqu iz";
    let mut entries: Vec<Vec<u8>> = (0..4000).map(|_| (0..(4 + next() % 40)).map(|_| letters[(next() % letters.len() as u64) as usize]).collect()).collect();
    for e in ["quiz", "xquizx", "qu iz", "zzquizquiz", "jinx", "jazz"] {
        entries.push(e.as_bytes().to_vec());
    }
    entries.sort();
    entries.dedup();
    let payload = Payload::of(&entries);
    let dict = common::VecDict { entries: entries.clone(), null: None };
    let mut buf = Buf::new(1 << 20);
    for p in ["%quiz%", "%qz%", "%zq%", "%jinx%", "%u iz%", "%xjqz%", "%qu%iz%", "%zq%zq%", "q%z%x", "%jinx%jazz", "%z%q%u%i%"] {
        for negated in [false, true] {
            for rule in [(|_, r, _, _| r > 0) as fn(u64, u64, u64, u64) -> bool, |_, _, _, _| false, |_, _, _, _| true] {
                let mut r = buf.region();
                let mut m = Matcher::new(p.as_bytes(), Mode::Like, &mut r).unwrap().recounting(rule);
                if negated {
                    m = m.negated();
                }
                let over = bits_of(&m.build_bits_over(&payload, &payload.bytes, &mut r).unwrap());
                let per = bits_of(&m.build_bits(&dict, &mut r).unwrap());
                for (c, e) in entries.iter().enumerate() {
                    let oracle = adt_like::utf8_match_text(e, p.as_bytes(), None).unwrap() == adt_like::LIKE_TRUE;
                    assert_eq!(over[c], oracle != negated, "{p:?} negated={negated} on {:?}", String::from_utf8_lossy(e));
                }
                assert_eq!(over, per, "{p:?} negated={negated}");
            }
        }
    }
}

/// D3288: Segments patterns (two or more runs between `%`s, anchored or not, escaped `%` / `_` inside a run, multi-byte
/// runs, runs that overlap or repeat) streamed over generated payloads equal the per-entry build and `adt_like`, negated
/// too; a pattern with `_` stays the general class (a run led by a continuation byte: like_corpus).
#[test]
fn segments_stream_over_the_payload_like_the_per_entry_build() {
    use gravity_dict::like::Class;
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let alphabet: [&str; 9] = ["a", "b", "ab", "ba", "%", "_", "é", "日", " special "];
    let mut entries: Vec<Vec<u8>> = (0..6000).map(|_| (0..(next() % 9)).flat_map(|_| alphabet[(next() % alphabet.len() as u64) as usize].bytes()).collect()).collect();
    for e in ["", "a", "ab", "aba", "abab", "aaa", "special requests", "requests special", "specialrequests", " special  special ", "a%b", "a_b", "é日é", "日é日"] {
        entries.push(e.as_bytes().to_vec());
    }
    entries.sort();
    entries.dedup();
    let payload = Payload::of(&entries);
    let dict = common::VecDict { entries: entries.clone(), null: None };
    let mut buf = Buf::new(1 << 20);
    let segs = ["%a%b%", "%ab%ab%", "%ba%ab%", "%a%a%a%", "a%b", "a%b%", "%a%b", "ab%ba", "%special%requests%", "%requests%special%", "%é%日%", "%日%é", "é%日", "%a\\%%b%", "%\\_%a%", "%%a%%b%%", "%sp%al%"];
    let general = ["%a_b%", "%a%_%"];
    let mut streamed = 0;
    for p in segs.iter().map(|p| p.as_bytes().to_vec()).chain(general.iter().map(|p| p.as_bytes().to_vec())) {
        for negated in [false, true] {
            for rule in [None, Some((|_, r, _, _| r > 0) as fn(u64, u64, u64, u64) -> bool)] {
                let mut r = buf.region();
                let mut m = Matcher::new(&p, Mode::Like, &mut r).unwrap();
                if let Some(rule) = rule {
                    m = m.recounting(rule);
                }
                if negated {
                    m = m.negated();
                }
                let over = bits_of(&m.build_bits_over(&payload, &payload.bytes, &mut r).unwrap());
                let per = bits_of(&m.build_bits(&dict, &mut r).unwrap());
                for (c, e) in entries.iter().enumerate() {
                    let oracle = adt_like::utf8_match_text(e, &p, None).unwrap() == adt_like::LIKE_TRUE;
                    assert_eq!(over[c], oracle != negated, "{:?} negated={negated} on {:?}", String::from_utf8_lossy(&p), String::from_utf8_lossy(e));
                }
                assert_eq!(over, per, "{:?} negated={negated}", String::from_utf8_lossy(&p));
                streamed += (m.class() == Class::Segments) as usize;
            }
        }
    }
    assert_eq!(streamed, segs.len() * 4, "every Segments pattern classifies as Segments; the general ones do not");
}
