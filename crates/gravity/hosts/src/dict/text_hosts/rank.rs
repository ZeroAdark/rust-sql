//! OID-19: a bpchar code lane's rank table (`TableFn::RankBpchar`). A part's codes are its entries' byte ranks, which is
//! `bpcharcmp`'s order only when no two entries differ by trailing blanks against a byte under 0x20 ('a  ' sorts after
//! 'a' || chr(9) by bytes, before it once trimmed). Each entry's value is its rank among the part's trimmed images
//! (`bcTruelen`), entries equal after the trim sharing one rank.
use gravity_dict::{Codes, Dict, Region, I64};
use gravity_fn::arena::{AVec, OpArena};

use super::raw::view_bytes;
use super::{arena_of, refused, BuildOwner, Grow, Stop, TableBuildError};

/// `b` without its trailing blanks (`bcTruelen`): a bpchar entry's order and equality bytes.
pub(super) fn trimmed(b: &[u8]) -> &[u8] {
    &b[..b.iter().rposition(|&c| c != b' ').map_or(0, |i| i + 1)]
}

/// Each code's rank among the distinct trimmed images `sweep` hands out (a code it never names: 0), one word per code on
/// `out`. The images are copied back to back into one buffer and each code's `(offset, length, code)` kept beside it, both
/// on arenas charged to `owner` (none: to no owner) and freed once the ranks are made.
pub(super) fn ranks(out: &OpArena, owner: BuildOwner, ncodes: u32, sweep: &mut dyn FnMut(&mut dyn FnMut(u32, &[u8]))) -> Result<AVec<i64>, TableBuildError> {
    let ia = arena_of(owner);
    let mut images: AVec<(usize, u32, u32)> = AVec::with_capacity(&ia, ncodes as usize).map_err(refused)?;
    let mut bytes: Grow<u8> = Grow::new(owner);
    let mut step = cancelpoll::step("text_hosts rank: a bpchar part's trimmed images copied");
    let mut stopped: Option<Stop> = None;
    sweep(&mut |c, e| {
        if stopped.is_some() {
            return;
        }
        let (t, at) = (trimmed(e), bytes.len());
        let put = bytes.reserve(&mut step, t.len()).and_then(|()| bytes.extend_from_slice(t)).and_then(|()| images.push(&ia, (at, t.len() as u32, c)).map_err(Stop::Refused));
        stopped = put.err();
    });
    if let Some(s) = stopped {
        return Err(s.into());
    }
    let b = bytes.as_slice();
    let image = |x: &(usize, u32, u32)| &b[x.0..x.0 + x.1 as usize];
    images.as_mut_slice(&ia).sort_unstable_by(|x, y| image(x).cmp(image(y)).then(x.2.cmp(&y.2)));
    let mut by_code: AVec<i64> = AVec::default();
    by_code.resize(out, ncodes as usize, 0).map_err(refused)?;
    let (o, imgs) = (by_code.as_mut_slice(out), images.as_slice(&ia));
    let mut rank = -1i64;
    for (i, x) in imgs.iter().enumerate() {
        rank += (i == 0 || image(x) != image(&imgs[i - 1])) as i64;
        o[x.2 as usize] = rank;
    }
    Ok(by_code)
}

/// The `I64` rank table of `dict` in `region`, from one sweep of its entries (a verbatim part's decodes its rows); its
/// working memory charged to `owner`.
pub(super) fn build(dict: &dyn Dict, owner: BuildOwner, region: &mut Region) -> Result<I64, TableBuildError> {
    let null = dict.null_code();
    let a = arena_of(owner);
    let by_code = ranks(&a, owner, dict.ncodes(), &mut |f| {
        dict.entries(&mut |c, v| {
            if Some(c) != null {
                f(c, view_bytes(v));
            }
        })
    })?;
    let by_code = by_code.as_slice(&a);
    Ok(I64::build_by_code(&Codes { ncodes: dict.ncodes(), null }, region, &mut |c| Ok(by_code[c as usize]))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_dict::View;

    struct Entries<'a>(&'a [&'a [u8]], Option<u32>);

    impl Dict for Entries<'_> {
        fn ncodes(&self) -> u32 {
            self.0.len() as u32
        }
        fn null_code(&self) -> Option<u32> {
            self.1
        }
        fn entries(&self, f: &mut dyn FnMut(u32, View)) {
            for (c, e) in self.0.iter().enumerate() {
                f(c as u32, View::of(e));
            }
        }
    }

    fn table(entries: &[&[u8]], null: Option<u32>) -> Vec<i64> {
        let mut buf = vec![0u64; 1 << 12]; // alloc: Cold
        let mut region = super::super::raw::region(&mut buf);
        let t = build(&Entries(entries, null), None, &mut region).expect("built");
        assert!(!t.has_faults());
        super::super::raw::i64_values(&t)
    }

    #[test]
    fn a_tab_sorts_after_the_padded_a() {
        // a part's byte order: '', ' ', 'a', 'a' || chr(9), 'a  ', 'b'
        assert_eq!(table(&[b"", b" ", b"a", b"a\t", b"a  ", b"b"], None), [0, 0, 1, 2, 1, 3]);
    }

    #[test]
    fn entries_equal_after_the_trim_share_one_rank() {
        assert_eq!(table(&[b"a", b"a ", b"a   "], None), [0, 0, 0]);
        assert_eq!(table(&[b"", b" ", b"   ", b"x"], None), [0, 0, 0, 1]);
    }

    #[test]
    fn an_empty_part_has_no_ranks() {
        assert!(table(&[], None).is_empty());
    }

    #[test]
    fn only_trailing_blanks_trim() {
        // D767: leading and interior blanks, a tab and an NBSP stay
        assert_eq!(trimmed(b" a b  "), b" a b");
        assert_eq!(trimmed(b"a\t"), b"a\t");
        assert_eq!(trimmed("a\u{a0}".as_bytes()), "a\u{a0}".as_bytes());
        assert_eq!(table(&[b" a", b"a", b"a b", b"a b "], None), [0, 1, 2, 2]);
        assert_eq!(table(&[b"a  ", b"a!", b"ab", b"ab  "], None), [0, 1, 2, 2]);
    }

    #[test]
    fn a_null_code_takes_no_rank() {
        assert_eq!(table(&[b"\0", b"a", b"b"], Some(0))[1..], [0, 1]);
    }

    /// The build's images and ranks are its owner's while it runs (the account's kind lane peaks at them) and gone after;
    /// the ranks are the same as with no owner.
    #[test]
    fn a_rank_builds_working_memory_is_its_owners_while_it_runs() {
        use memledger::{charge::usage, Kind};
        super::super::owner_tests::in_statement(3974, Kind::TopK, |a, slot| {
            let vals: Vec<Vec<u8>> = (0..50_000u32).map(|i| format!("{:08}   ", 49_999 - i).into_bytes()).collect(); // alloc: Cold
            let entries: Vec<&[u8]> = vals.iter().map(|v| &v[..]).collect();
            let ranked = |owner| {
                let mut buf = vec![0u64; 1 << 17]; // alloc: Cold
                let mut region = super::super::raw::region(&mut buf);
                super::super::raw::i64_values(&build(&Entries(&entries, None), owner, &mut region).expect("built"))
            };
            let before = usage(slot).held;
            let ranks = ranked(Some((slot, Kind::TopK)));
            assert_eq!(usage(slot).held, before, "gone after the build");
            assert_eq!(ranks, (0..50_000i64).map(|c| 49_999 - c).collect::<Vec<_>>());
            assert_eq!(ranks, ranked(None), "as with no owner");
            let mut peak = 0;
            memledger::for_each_kind_row(|r| {
                if r.acct == a && r.kind == Kind::TopK {
                    peak = r.peak_bytes;
                }
            });
            // 50,000 triples of 16 bytes, the trimmed images' 400,000 bytes, 50,000 ranks of 8 bytes
            assert!(peak >= 50_000 * 16 + 400_000 + 50_000 * 8, "the build's working memory on its owner at its peak: {peak}");
        });
    }

    #[test]
    fn codes_out_of_byte_order_rank_by_image() {
        // a sweep in any code order (a verbatim part hands each code's representative row as it meets it)
        let a = arena_of(None);
        let r = ranks(&a, None, 4, &mut |f| {
            f(3, b"b");
            f(0, b"a  ");
            f(2, b"a\t");
            f(1, b"a");
        })
        .expect("ranked");
        assert_eq!(r.as_slice(&a), [0, 0, 1, 2]);
    }
}
