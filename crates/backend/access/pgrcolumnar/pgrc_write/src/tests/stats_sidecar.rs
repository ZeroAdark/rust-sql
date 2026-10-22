//! ST-1/ST-2 (OD-2, M3-L2): stats are a SEAL byproduct — every published
//! part carries the Stats sidecar (MCV top-k exact counts + histogram
//! bounds; HLL stays in-footer), and the ANALYZE fold produces
//! pg_statistic-shaped output from seal-built facts alone (the
//! estimate-consumer probe's precondition: never-analyzed refusal is
//! unreachable on banked data).

use super::*;
use crate::structural::{ClusterKeyDecl, NullsOrder, SortDir, StructuralPolicy};
use pgrc_format::dirlayout::sidecar_file_name;
use pgrc_format::meta::{StatsRecord, STATS_RECORD_LEN};
use pgrc_format::sidecar::{decode_stats_payload, SidecarFileHeader, SidecarKind};
use pgrc_meta::fold::{fold_pg_statistic, PartColInput};

const FXID: u64 = 88;

fn seal_and_publish(n: u64, cluster: bool) -> (MemVfs, u64) {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let mut w = open_writer_policy(
        vec![int8_col(1), text_col(2)],
        stamp(FXID, 1),
        PartCutPolicy {
            max_rows: u64::MAX,
            max_bytes: u64::MAX,
            cut_granule_rows: 128,
        },
    );
    if cluster {
        w.set_structural(StructuralPolicy::new().with_cluster_key(vec![ClusterKeyDecl {
            attno: 1,
            dir: SortDir::Asc,
            nulls: NullsOrder::Last,
            path: Vec::new(),
        }]));
    }
    for i in 0..n {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        with_mixed_row(i, |row| w.append_row(row, &mut kit.ext, &mut env).expect("append"));
    }
    let probe = Probe::new(TxnVerdict::Committed);
    // The publish installs each part's Stats companion under its own record.
    {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.finish(&mut env).expect("finish");
    }
    w.publish(&mut vfs, &probe).expect("publish");
    (vfs, n)
}

/// Decode the part-grain StatsRecord of (attno, path_ord) from the sealed
/// part (Stats section: granules, bands, then the part record).
fn part_stats_record(pv: &PartView, attno: u32) -> StatsRecord {
    let body = pv.meta_body(SectionKind::Stats, attno, 0).expect("Stats");
    assert_eq!(body.len() % STATS_RECORD_LEN, 0);
    let recs = body.len() / STATS_RECORD_LEN;
    let mut c = Cur::new(&body[(recs - 1) * STATS_RECORD_LEN..]);
    StatsRecord::decode(&mut c).expect("part record")
}

#[test]
fn published_part_carries_the_stats_sidecar() {
    let (mut vfs, n) = seal_and_publish(600, false);
    let name = sidecar_file_name(0, SidecarKind::Stats, 1);
    let image = vfs
        .read_full(&format!("{DIR}/{name}"))
        .expect("stats sidecar file exists — stats are a seal byproduct");
    let (hdr, payload) = SidecarFileHeader::validate_file(&image).expect("envelope validates");
    assert_eq!(hdr.kind, SidecarKind::Stats.as_u16());
    let cols = decode_stats_payload(payload).expect("payload decodes");
    // Both streams computed sketches (int8 + C-collated text profiles are
    // ndv-armed).
    let int8 = cols
        .iter()
        .find(|(a, p, _)| (*a, *p) == (1, 0))
        .map(|(_, _, d)| d)
        .expect("int8 sketch");
    let text = cols
        .iter()
        .find(|(a, p, _)| (*a, *p) == (2, 0))
        .map(|(_, _, d)| d)
        .expect("text sketch");
    // mixed_row: int8 null every 13th, text null every 17th; int8 values
    // unique, text values i % 977.
    let int8_nonnull = (0..n).filter(|i| i % 13 != 0).count() as u64;
    let text_nonnull = (0..n).filter(|i| i % 17 != 0).count() as u64;
    assert_eq!(int8.nonnull, int8_nonnull);
    assert_eq!(text.nonnull, text_nonnull);
    assert_eq!(int8.ndv_eligible, int8_nonnull, "unique int8 keys: NDV exact");
    assert_eq!(int8.long_values, 0);
    // MCV counts are EXACT: every int8 value occurs once.
    assert!(int8.mcv.iter().all(|(_, c)| *c == 1));
    // text: 600 rows over 977 residues — every present residue counted
    // exactly; the top MCV count matches a direct count of the feed.
    let (top_val, top_count) = &text.mcv[0];
    let expect: u64 = (0..n)
        .filter(|i| i % 17 != 0)
        .filter(|i| format!("value-{}", i % 977).as_bytes() == top_val.as_slice())
        .count() as u64;
    assert_eq!(*top_count, expect, "MCV counts are exact at seal");
    // Histogram bounds are byte-ordered with min first / max last.
    assert!(!text.hist_bounds.is_empty());
    let mut sorted = text.hist_bounds.clone();
    sorted.sort();
    assert_eq!(sorted, text.hist_bounds);
}

#[test]
fn fold_produces_pg_statistic_shaped_output() {
    let (mut vfs, n) = seal_and_publish(600, true);
    let name = sidecar_file_name(0, SidecarKind::Stats, 1);
    let image = vfs.read_full(&format!("{DIR}/{name}")).expect("sidecar");
    let (_, payload) = SidecarFileHeader::validate_file(&image).expect("envelope");
    let cols = decode_stats_payload(payload).expect("payload");
    let pv = PartView::open(&mut vfs, "part-0.pgrc");
    let int8_rec = part_stats_record(&pv, 1);
    let int8_sketch = cols
        .iter()
        .find(|(a, p, _)| (*a, *p) == (1, 0))
        .map(|(_, _, d)| d)
        .expect("int8 sketch");
    let out = fold_pg_statistic(
        &[PartColInput {
            part_record: &int8_rec,
            rows: n,
            sketch: Some(int8_sketch),
        }],
        int8_sketch.ndv_eligible as f64,
        true, // FT-6 witness: the cluster key is declared on attno 1
        pgrc_meta::fold::HistOrder::Signed,
    )
    .expect("fold");
    let nullfrac_expect = 1.0 - (int8_rec.nonnull as f64 / n as f64);
    assert!((out.stanullfrac - nullfrac_expect).abs() < 1e-9);
    assert!(out.stadistinct > 0.0);
    // FOLDSTATS: a unique column has no MCV list (compute_scalar_stats keeps a value only if it repeats) and its
    // histogram is in value order
    assert!(out.sta_mcv.is_empty(), "unique int8 keys: no MCVs");
    let words: Vec<i64> = out.sta_histogram.iter().map(|b| i64::from_le_bytes(b.as_slice().try_into().expect("int8 bound"))).collect();
    assert!(words.len() >= 2 && words.windows(2).all(|w| w[0] < w[1]), "value-ordered bounds: {words:?}");
    assert_eq!(
        out.stacorrelation, 1.0,
        "P-8: the declared-and-verified cluster key folds to +1.0, never default-0"
    );
    // ST-2's refusal law, witnessed from the consumer side: the fold is
    // total on seal-built inputs — a sealed part ALWAYS folds.
}

/// Born-RED (the stats-stripped shape): a sidecar image with a corrupted
/// byte fails validation typed — the estimate probe's stripped-bank seed
/// fails through exactly this face.
#[test]
fn corrupted_sidecar_refuses_typed() {
    let (mut vfs, _) = seal_and_publish(300, false);
    let name = sidecar_file_name(0, SidecarKind::Stats, 1);
    let mut image = vfs.read_full(&format!("{DIR}/{name}")).expect("sidecar");
    let mid = image.len() / 2;
    image[mid] ^= 0xFF;
    assert!(
        SidecarFileHeader::validate_file(&image).is_err(),
        "a stripped/corrupt stats companion must refuse, never serve numbers"
    );
}
