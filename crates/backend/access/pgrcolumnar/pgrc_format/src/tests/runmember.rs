//! SORTRUNS-E0 L0 (pgrc-plans/SORTRUNS-E0-STEP1.md D2): a sorted run's member's map (`RevForm::Run`). The run's
//! reverse names a member's virtual frame; the map answers an ID among a LABELS group's virtual frames (the fast path:
//! the candidate by fence minimum and its neighbours) and in one virtual frame (an exception, a FRAMEIX run's frame).
//! A member rewritten in place (rows dropped, a virtual frame emptied) keeps its virtual frames, so every answer the
//! run's unchanged reverse leads to still holds. The member's own lookup goes through a built FRAMEIX reverse.

use crate::rowid::ROWID_LIMIT;
use crate::rowmap::{encode_run_member, run_cand, summary, GranMap, RevForm, GRANULE, G_DELTA, RUN_FRAME_LARGE, RUN_FRAME_SMALL};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A LABELS run's member as the run builder would cut it: `rows` rows of `e` groups (each group's IDs ascending, with
/// `late` per mille displaced), virtual frames cut at group starts and every [`RUN_FRAME_LARGE`] rows, each frame's
/// fence minimum. The IDs, the virtual frame starts, their minimums and each group's virtual frame span.
fn labels_member(rng: &mut Rng, rows: usize, e: u64, late: u64) -> (Vec<u64>, Vec<u32>, Vec<u64>, Vec<(u32, u32)>) {
    let mut keyed: Vec<(u64, u64, u64)> = (0..rows as u64)
        .map(|t| {
            let id = 291 * 4096 + t * 3 + rng.below(2);
            let t2 = if rng.below(1000) < late { t.saturating_sub(rng.below(400)) } else { t };
            (rng.below(e), t2, id)
        })
        .collect();
    keyed.sort_unstable();
    let ids: Vec<u64> = keyed.iter().map(|k| k.2).collect();
    let (mut vstarts, mut spans) = (Vec::new(), Vec::new());
    let mut p = 0usize;
    while p < rows {
        let g = keyed[p].0;
        let end = p + keyed[p..].partition_point(|k| k.0 == g);
        let va = vstarts.len() as u32;
        let mut s = p;
        while s < end {
            vstarts.push(s as u32);
            s = (s + RUN_FRAME_LARGE as usize).min(end);
        }
        spans.push((va, vstarts.len() as u32));
        p = end;
    }
    let vmin: Vec<u64> = (0..vstarts.len()).map(|v| ids[vstarts[v] as usize..vstarts.get(v + 1).map_or(rows, |&x| x as usize)].iter().copied().min().unwrap()).collect();
    (ids, vstarts, vmin, spans)
}

/// The virtual frame of row `o`.
fn vframe_of(vstarts: &[u32], o: usize) -> u32 {
    (vstarts.partition_point(|&s| s as usize <= o) - 1) as u32
}

/// Every row through its group's fast path or (missed: the run lists it as an exception) its own virtual frame; the
/// rows the fast path found.
fn check_member(m: &GranMap, ids: &[u64], vstarts: &[u32], spans: &[(u32, u32)], group_of: &dyn Fn(usize) -> usize) -> Vec<bool> {
    assert_eq!(m.rev_form(), RevForm::Run);
    assert_eq!(m.vframes().unwrap() as usize, vstarts.len());
    let mut fast = vec![false; ids.len()];
    for (o, &id) in ids.iter().enumerate() {
        let (va, vb) = spans[group_of(o)];
        match m.run_find(va, vb, id).unwrap() {
            Some(x) => {
                assert_eq!(x as usize, o, "fast path for {id}");
                fast[o] = true;
            }
            None => assert_eq!(m.run_find_in(vframe_of(vstarts, o), id).unwrap(), Some(o as u32), "exception path for {id}"),
        }
        assert_eq!(m.run_find_in(vframe_of(vstarts, o), id).unwrap(), Some(o as u32), "its virtual frame holds {id}");
    }
    fast
}

#[test]
fn runmember_labels_member_answers_through_its_groups() {
    let mut rng = Rng(7);
    let (ids, vstarts, vmin, spans) = labels_member(&mut rng, 30_000, 40, 20);
    let body = encode_run_member(&ids, &vstarts, Some(&vmin)).unwrap();
    let m = GranMap::decode(&body, ids.len() as u64).unwrap();
    assert!(m.frames() > vstarts.len(), "virtual frames across a granule end take two forward frames");
    let group_of = |o: usize| spans.partition_point(|s| (vstarts[s.0 as usize] as usize) <= o) - 1;
    let fast = check_member(&m, &ids, &vstarts, &spans, &group_of);
    let missed = fast.iter().filter(|f| !**f).count();
    assert!(missed * 20 < ids.len(), "the fast path finds most rows ({missed} missed)");
    let mut all = vec![0u64; ids.len()];
    m.decode_ids(0, &mut all);
    assert_eq!(all, ids, "forward decode");
    // absent IDs: below, above and between
    for id in [0, 291 * 4096 - 1, ids.iter().max().unwrap() + 1, ROWID_LIMIT - 1] {
        for &(va, vb) in spans.iter().step_by(7) {
            assert_eq!(m.run_find(va, vb, id).unwrap(), None);
        }
    }
    // the member's own lookup: a built FRAMEIX reverse
    assert!(m.needs_rev());
    assert_eq!(m.ordinal_of(ids[5], None), None, "no reverse of its own");
    let rev = m.build_frameix().unwrap();
    for (o, &id) in ids.iter().enumerate().step_by(13) {
        assert_eq!(m.ordinal_of(id, Some(&rev)), Some(o as u32));
    }
    let mut out = vec![None; 200];
    m.ordinals_of(&ids[..200], Some(&rev), &mut out);
    assert!(out.iter().enumerate().all(|(o, x)| *x == Some(o as u32)), "batch");
    let (rf, forms) = summary(&body).unwrap();
    assert_eq!(rf, RevForm::Run);
    assert_eq!(forms[G_DELTA as usize] as usize, ids.len().div_ceil(GRANULE as usize));
    // a range past the map's virtual frames is corrupt, not an answer
    let nv = vstarts.len() as u32;
    assert!(m.run_find(nv - 1, nv + 1, ids[0]).is_err());
    assert!(m.run_find(3, 3, ids[0]).is_err());
    assert!(m.run_find_in(nv, ids[0]).is_err());
    assert_eq!(run_cand(&vmin[..1], ids[0]), 0);
}

/// A member rewritten in place: every third row and one whole virtual frame dropped, the virtual frames and their
/// fence minimums kept. Every surviving row the fast path found before is found again under its new ordinal through
/// the same group spans; the dropped rows are gone; the emptied frame has no forward frame.
#[test]
fn runmember_a_rewrite_keeps_the_virtual_frames() {
    let mut rng = Rng(11);
    let (ids, vstarts, vmin, spans) = labels_member(&mut rng, 20_000, 25, 20);
    let group_of = |o: usize| spans.partition_point(|s| (vstarts[s.0 as usize] as usize) <= o) - 1;
    let m = GranMap::decode(&encode_run_member(&ids, &vstarts, Some(&vmin)).unwrap(), ids.len() as u64).unwrap();
    let fast = check_member(&m, &ids, &vstarts, &spans, &group_of);
    let gone_v = 5u32;
    let keep = |o: usize| o % 3 != 0 && vframe_of(&vstarts, o) != gone_v;
    let kept: Vec<usize> = (0..ids.len()).filter(|&o| keep(o)).collect();
    let new_ids: Vec<u64> = kept.iter().map(|&o| ids[o]).collect();
    let new_starts: Vec<u32> = vstarts.iter().map(|&s| kept.partition_point(|&o| o < s as usize) as u32).collect();
    assert_eq!(new_starts[gone_v as usize], new_starts[gone_v as usize + 1], "an emptied virtual frame");
    let body = encode_run_member(&new_ids, &new_starts, Some(&vmin)).unwrap();
    let r = GranMap::decode(&body, new_ids.len() as u64).unwrap();
    assert_eq!(r.vframes().unwrap() as usize, vstarts.len(), "the virtual frames survive");
    for (n, &o) in kept.iter().enumerate() {
        let (va, vb) = spans[group_of(o)];
        let id = ids[o];
        if fast[o] {
            assert_eq!(r.run_find(va, vb, id).unwrap(), Some(n as u32), "kept row {id} through its group");
        }
        assert_eq!(r.run_find_in(vframe_of(&vstarts, o), id).unwrap(), Some(n as u32), "kept row {id} in its frame");
    }
    for o in (0..ids.len()).filter(|&o| !keep(o)).step_by(5) {
        let (va, vb) = spans[group_of(o)];
        assert_eq!(r.run_find(va, vb, ids[o]).unwrap(), None, "dropped row {}", ids[o]);
        assert_eq!(r.run_find_in(vframe_of(&vstarts, o), ids[o]).unwrap(), None);
    }
    // trailing virtual frames emptied
    let head: Vec<u64> = ids[..vstarts[3] as usize].to_vec();
    let starts: Vec<u32> = vstarts.iter().map(|&s| s.min(vstarts[3])).collect();
    let t = GranMap::decode(&encode_run_member(&head, &starts, Some(&vmin)).unwrap(), head.len() as u64).unwrap();
    assert_eq!(t.vframes().unwrap() as usize, vstarts.len());
    assert_eq!(t.run_find_in(3, ids[vstarts[3] as usize]).unwrap(), None);
    assert_eq!(t.run_find_in(1, ids[vstarts[1] as usize]).unwrap(), Some(vstarts[1]));
}

/// A FRAMEIX run's member: small virtual frames, no fence minimums; answers in one virtual frame.
#[test]
fn runmember_frameix_member_answers_in_its_frames() {
    let mut rng = Rng(13);
    let mut ids: Vec<u64> = (0..12_000u64).map(|k| 291 + k * 2).collect();
    for i in (1..ids.len()).rev() {
        ids.swap(i, rng.below(i as u64 + 1) as usize);
    }
    let vstarts: Vec<u32> = (0..ids.len() as u32).step_by(RUN_FRAME_SMALL as usize).collect();
    let m = GranMap::decode(&encode_run_member(&ids, &vstarts, None).unwrap(), ids.len() as u64).unwrap();
    assert_eq!(m.vframes().unwrap() as usize, vstarts.len());
    for (o, &id) in ids.iter().enumerate() {
        assert_eq!(m.run_find_in(vframe_of(&vstarts, o), id).unwrap(), Some(o as u32));
    }
    assert_eq!(m.run_find_in(0, ids[RUN_FRAME_SMALL as usize]).unwrap(), None, "another frame's ID");
    assert!(m.run_find(0, 2, ids[0]).is_err(), "no fence minimums: a LABELS group search is corrupt");
}

#[test]
fn runmember_encode_refuses_what_a_run_never_writes() {
    let ids: Vec<u64> = (0..1000u64).map(|k| 291 + 7 * k).collect();
    let vs: Vec<u32> = vec![0, 400, 800];
    let vm: Vec<u64> = vs.iter().map(|&s| ids[s as usize]).collect();
    assert!(encode_run_member(&ids, &vs, Some(&vm)).is_ok());
    assert!(encode_run_member(&[], &[0], Some(&[0])).is_err(), "no rows");
    assert!(encode_run_member(&ids, &[], Some(&[])).is_err(), "no virtual frame");
    assert!(encode_run_member(&ids, &[1, 400, 800], Some(&vm)).is_err(), "not from row 0");
    assert!(encode_run_member(&ids, &[0, 800, 400], Some(&vm)).is_err(), "not ascending");
    assert!(encode_run_member(&ids, &[0, 400, 1001], Some(&vm)).is_err(), "past the rows");
    assert!(encode_run_member(&ids, &[0, 600], Some(&vm[..2])).is_err(), "a frame past 512 rows");
    assert!(encode_run_member(&ids, &vs, None).is_err(), "a FRAMEIX member's frames past 128 rows");
    assert!(encode_run_member(&ids, &vs, Some(&vm[..2])).is_err(), "a minimum per frame");
    assert!(encode_run_member(&ids, &vs, Some(&[vm[0], vm[1] + 1, vm[2]])).is_err(), "a minimum above a row");
    assert!(encode_run_member(&[ROWID_LIMIT], &[0], Some(&[0])).is_err(), "an ID past the space");
}

#[test]
fn runmember_corrupt_bodies_are_refused_and_never_panic() {
    let mut rng = Rng(17);
    let (ids, vstarts, vmin, spans) = labels_member(&mut rng, 12_000, 9, 30);
    let n = ids.len() as u64;
    for body in [encode_run_member(&ids, &vstarts, Some(&vmin)).unwrap(), encode_run_member(&ids, &(0..n as u32).step_by(128).collect::<Vec<_>>(), None).unwrap()] {
        assert!(GranMap::decode(&body, n + 1).is_err(), "rows mismatch");
        assert!(GranMap::decode(&body[..body.len() - 1], n).is_err(), "truncated");
        let mut long = body.clone();
        long.push(0);
        assert!(GranMap::decode(&long, n).is_err(), "trailing bytes");
        // the tail holds the virtual frames: damage there is refused or harmless
        for _ in 0..600 {
            let mut b = body.clone();
            for _ in 0..1 + rng.below(3) {
                let i = if rng.below(2) == 0 { b.len() - 1 - rng.below(b.len().min(4096) as u64) as usize } else { rng.below(b.len() as u64) as usize };
                b[i] ^= 1 << rng.below(8);
            }
            if let Ok(m) = GranMap::decode(&b, n) {
                let nv = m.vframes().unwrap_or(0);
                for (o, &id) in ids.iter().enumerate().step_by(41) {
                    let (va, vb) = spans[spans.partition_point(|s| (vstarts[s.0 as usize] as usize) <= o) - 1];
                    let _ = m.run_find(va, vb, id);
                    let _ = m.run_find_in(vframe_of(&vstarts, o).min(nv.saturating_sub(1)), id);
                }
                let rev = m.build_frameix().ok();
                let _ = m.ordinal_of(ids[0], rev.as_ref());
            }
        }
    }
}

/// SORTRUNS-E0 L3b1: a member's virtual frames read back from its map (`run_vframes`) are the ones it was written
/// with: starts and fence minimums for a LABELS run's member, starts alone for a FRAMEIX run's, and after a rewrite in
/// place (emptied frames repeat the next start; trailing ones the rows) the rewrite's. A member rewritten from them
/// again encodes the same body. Any other map form has none.
#[test]
fn runmember_virtual_frames_read_back() {
    let mut rng = Rng(19);
    let (ids, vstarts, vmin, _) = labels_member(&mut rng, 9_000, 12, 20);
    let body = encode_run_member(&ids, &vstarts, Some(&vmin)).unwrap();
    let m = GranMap::decode(&body, ids.len() as u64).unwrap();
    assert_eq!(m.run_vframes().unwrap(), (vstarts.clone(), Some(vmin.clone())));
    // a rewrite in place: every fourth row, all of virtual frame 2 and the last two frames dropped
    let nv = vstarts.len();
    let gone = |v: u32| v == 2 || v as usize >= nv - 2;
    let kept: Vec<usize> = (0..ids.len()).filter(|&o| o % 4 != 0 && !gone(vframe_of(&vstarts, o))).collect();
    let new_ids: Vec<u64> = kept.iter().map(|&o| ids[o]).collect();
    let new_starts: Vec<u32> = vstarts.iter().map(|&s| kept.partition_point(|&o| o < s as usize) as u32).collect();
    let r = GranMap::decode(&encode_run_member(&new_ids, &new_starts, Some(&vmin)).unwrap(), new_ids.len() as u64).unwrap();
    let (starts, mins) = r.run_vframes().unwrap();
    assert_eq!((starts.clone(), mins.clone()), (new_starts.clone(), Some(vmin.clone())));
    assert_eq!(starts[2], starts[3], "the emptied frame repeats the next start");
    assert_eq!(starts[nv - 1], new_ids.len() as u32, "a trailing emptied frame starts at the rows");
    assert_eq!(encode_run_member(&new_ids, &starts, mins.as_deref()).unwrap(), encode_run_member(&new_ids, &new_starts, Some(&vmin)).unwrap());
    // a FRAMEIX run's member
    let fids: Vec<u64> = (0..3_000u64).map(|k| 291 + k * 5).collect();
    let fstarts: Vec<u32> = (0..fids.len() as u32).step_by(RUN_FRAME_SMALL as usize).collect();
    let f = GranMap::decode(&encode_run_member(&fids, &fstarts, None).unwrap(), fids.len() as u64).unwrap();
    assert_eq!(f.run_vframes().unwrap(), (fstarts, None));
    // not a run member's map
    let plain = GranMap::decode(&crate::rowmap::encode(&fids, crate::rowmap::Opts::default()).unwrap(), fids.len() as u64).unwrap();
    assert!(plain.run_vframes().is_err());
}
