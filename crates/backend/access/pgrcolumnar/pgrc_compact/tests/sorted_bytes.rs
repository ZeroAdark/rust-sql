mod common;

use common::{cases, random_decl, ref_cmp, Rng};
use pgrc_compact::order::{concat_order, sorted_order, Input};
use pgrc_compact::RowMapView;
use pgrc_format::rowmap::{encode, GranMap, Opts};
use pgrc_write::elect::CandidateSource;
use pgrc_write::ingest::RawDatum;
use pgrc_write::seal::cmp::KeySlot;
use pgrc_write::structural::{ClusterKeyDecl, StructuralPolicy};
use pgrc_write::testkit::{img_4b_u, int8_col, mem_with_dir, open_writer_policy, stamp, text_col, Kit, DIR};
use pgrc_write::writer::{PartCutPolicy, SealEnv};
use pgrc_write::wvfs::WriteVfs;

type Row = (Option<i64>, i64, String);

fn seal(rows: &[&Row], decls: &[ClusterKeyDecl]) -> (Vec<u8>, Vec<u32>) {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let mut w = open_writer_policy(vec![int8_col(1), int8_col(2), text_col(3)], stamp(77, 1), PartCutPolicy { max_rows: u64::MAX, max_bytes: u64::MAX, cut_granule_rows: 128 });
    w.set_structural(StructuralPolicy::new().with_cluster_key(decls.to_vec()));
    for (k, v, t) in rows {
        let img = img_4b_u(t.as_bytes());
        let row = [k.map_or(RawDatum::Null, |k| RawDatum::Word(k as u64)), RawDatum::Word(*v as u64), RawDatum::Bytes(&img)];
        let sources: [&dyn CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
        w.append_row(&row, &mut kit.ext, &mut env).expect("append");
    }
    let sources: [&dyn CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
    w.finish(&mut env).expect("finish");
    assert_eq!(w.sealed_parts().len(), 1);
    let bytes = vfs.read_full(&format!("{DIR}/{}", w.sealed_parts()[0].tmp_name)).expect("tmp bytes");
    (bytes, w.seal_reports()[0].cluster_keys.clone())
}

#[test]
fn a_sorted_merge_feed_seals_like_a_presorted_copy() {
    for case in 0..cases(10_000) / 50 {
        let mut r = Rng(0x51_7cc1_b727_220a ^ (case + 7).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let rows: Vec<Vec<Row>> = (0..3).map(|p| (0..1 + r.below(300)).map(|i| ((!r.chance(10)).then(|| r.below(20) as i64 - 10), r.below(5) as i64, format!("p{p}-{i}-{}", r.below(1000)))).collect()).collect();
        let n = |p: usize| rows[p].len() as u32;
        let sealed: Vec<u64> = { let mut x = 0u64; (0..n(1)).map(|_| { x += 1 + r.below(4); 291 * 1000 + x }).collect() };
        let concat: Vec<u64> = (0..u64::from(n(2))).map(|i| 291 * 5000 + i).collect();
        let merged: Vec<u64> = (0..u64::from(n(2))).map(|i| ((i % 2) << 32) + u64::from(n(2)) - 1 - i).collect();
        let gran = |ids: &[u64]| GranMap::decode(&encode(ids, Opts::default()).expect("the map"), ids.len() as u64).expect("the map decodes");
        let (g1, g2) = (gran(&sealed), gran(if r.chance(50) { &concat } else { &merged }));
        let maps = [RowMapView::Implicit(291 * 10), RowMapView::Granules(&g1), RowMapView::Granules(&g2)];
        let dvs: Vec<Vec<bool>> = (0..3).map(|p| (0..n(p)).map(|_| r.chance(20)).collect()).collect();
        let dels: Vec<Box<dyn Fn(u32) -> bool + '_>> = dvs.iter().map(|d| Box::new(move |o: u32| d[o as usize]) as Box<dyn Fn(u32) -> bool + '_>).collect();
        let inputs: Vec<Input<'_>> = (0..3).map(|p| Input { part_no: [5, 9, 60][p], rows: n(p), deleted: &*dels[p], map: &maps[p] }).collect();
        let decls = if r.chance(50) { vec![random_decl(&mut r, 1)] } else { vec![random_decl(&mut r, 1), random_decl(&mut r, 2)] };
        let row = |&(p, o): &(u16, u32)| &rows[p as usize][o as usize];
        let base = concat_order(&inputs).unwrap();
        let keys: Vec<Vec<KeySlot<'_>>> = decls.iter().map(|d| base.iter().map(|x| match (d.attno, row(x)) {
            (1, (k, _, _)) => k.map_or(KeySlot::Null, KeySlot::I),
            (_, (_, v, _)) => KeySlot::I(*v),
        }).collect()).collect();
        let order = sorted_order(&inputs, &keys, &decls).unwrap();
        let key_of = |x: &(u16, u32), d: &ClusterKeyDecl| if d.attno == 1 { row(x).0 } else { Some(row(x).1) };
        let mut want = base.clone();
        want.sort_by(|a, b| {
            decls.iter().map(|d| ref_cmp(key_of(a, d), key_of(b, d), d)).find(|o| o.is_ne()).unwrap_or(inputs[a.0 as usize].ident(a.1).cmp(&inputs[b.0 as usize].ident(b.1)))
        });
        assert_eq!(order, want, "case {case}");
        let (got, attested) = seal(&order.iter().map(row).collect::<Vec<_>>(), &decls);
        let (reference, _) = seal(&want.iter().map(row).collect::<Vec<_>>(), &decls);
        assert!(got == reference, "case {case}: the parts' bytes differ");
        assert_eq!(attested, decls.iter().map(|d| d.attno).collect::<Vec<_>>(), "the clustered witness");
    }
}
