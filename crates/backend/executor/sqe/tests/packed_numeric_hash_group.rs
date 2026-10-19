//! sum/avg over a PackedNumeric column grouped by a sparse 8-byte key (the
//! hash route, not the dense direct array): the decoded word of a numeric
//! row is a pointer to its image, so the fold must read the mantissa
//! through the face, never the raw word.

use pgrc2_format::class::{CollationClass, ColSchema, StorageClass, TypeSemantics};
use pgrc2_write::elect::{CodecCandidates, ColumnPosture};
use pgrc2_write::ingest::RawDatum;
use pgrc2_write::publish::TxnVerdict;
use pgrc2_write::testkit::{Kit, Probe};
use pgrc2_write::writer::{PartCutPolicy, SealEnv, TableWriter, TxnStamp};
use pgrc2_write::wvfs::RealVfs;
use sqe::bank::{Bank, ColMeta, Face, OpenOpts};
use sqe::engine::{Engine, SqeConfig};
use sqe::planner::{plan_from_ap, AAgg, AFamily, AKeyExpr, APlan, AValExpr};
use sqe::render::to_lines;
use sqe::typmeta::TypMeta;

const ROWS: u64 = 6_000;
const GROUPS: u64 = 40;

fn key(i: u64) -> i64 {
    (i % GROUPS) as i64 * 999_999_999_997 - 3
}

fn mant(i: u64) -> i64 {
    (i % 997) as i64 * 7 + 125
}

fn dec2(m: i64) -> String {
    format!("{}.{:02}", m / 100, m % 100)
}

fn numeric_img(s: &str) -> Vec<u8> {
    adt_numeric::io::numeric_in(s, -1, None)
        .expect("parse")
        .expect("non-soft")
        .as_bytes()
        .to_vec()
}

fn seal_bank(dir: &str) {
    std::fs::create_dir_all(dir).expect("mkdir");
    let mut vfs = RealVfs;
    let mut kit = Kit::new();
    let schema = vec![
        ColSchema {
            attno: 1,
            class: StorageClass::ByvalWord { width: 8, signed: true },
            typlen: 8,
            typbyval: true,
            typalign: b'd',
            collation_class: CollationClass::C,
            semantics: TypeSemantics::SignedInt,
        },
        ColSchema {
            attno: 2,
            class: StorageClass::VarlenaVerbatim,
            typlen: -1,
            typbyval: false,
            typalign: b'i',
            collation_class: CollationClass::C,
            semantics: TypeSemantics::PackedNumeric { scale: 2 },
        },
    ];
    let cands = CodecCandidates::new(ColumnPosture::default()).with_column(
        2,
        0,
        ColumnPosture { numeric: true, ..ColumnPosture::default() },
    );
    let resolver = pgrc2_write::seal::CodecResolver;
    let mut w = TableWriter::open(
        dir.to_string(),
        schema,
        1663,
        5,
        791,
        TxnStamp { fxid: 100, cid: 1 },
        &Default::default(),
        PartCutPolicy { max_rows: 4096, max_bytes: u64::MAX, cut_granule_rows: 1024 },
    )
    .expect("open writer");
    for i in 0..ROWS {
        let img = numeric_img(&dec2(mant(i)));
        let datums = [RawDatum::Word(key(i) as u64), RawDatum::Bytes(&img)];
        let sources: [&dyn pgrc2_write::elect::CandidateSource; 1] = [&cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.append_row(&datums, &mut kit.ext, &mut env).expect("append");
    }
    {
        let sources: [&dyn pgrc2_write::elect::CandidateSource; 1] = [&cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.finish(&mut env).expect("finish");
    }
    let probe = Probe::new(TxnVerdict::Committed);
    w.publish(&mut vfs, &probe).expect("publish");
}

fn ap(agg: Vec<AAgg>) -> APlan {
    APlan {
        agg_filters: Vec::new(),
        sortagg_keys: Vec::new(),
        win: None,
        q: 0,
        family: AFamily::HashPlaneOwnedGroup,
        tags: Vec::new(),
        cols: vec![1, 2],
        pred: None,
        group: vec![AKeyExpr::Col(1)],
        agg,
        order: None,
        having: None,
        params: Vec::new(),
        fingerprints: Vec::new(),
        kernel_oracle: String::new(),
        notes: "packed_numeric_hash_group".into(),
        flags: Vec::new(),
    }
}

fn run(eng: &Engine, agg: Vec<AAgg>) -> Vec<String> {
    let node = plan_from_ap(&eng.bank, &eng.faces, &ap(agg)).expect("plan");
    let mut l = to_lines(&eng.run(&node));
    l.sort();
    l
}

fn expected() -> Vec<(i64, i64, i128)> {
    let mut g: Vec<(i64, i64, i128)> =
        (0..GROUPS).map(|j| (key(j), 0i64, 0i128)).collect();
    for i in 0..ROWS {
        let e = &mut g[(i % GROUPS) as usize];
        e.1 += 1;
        e.2 += mant(i) as i128;
    }
    g
}

#[test]
fn sparse_int8_key_sums_numeric_mantissas() {
    let dir = std::env::temp_dir().join(format!("sqe_packednum_hash_{}", std::process::id()));
    let dir = dir.to_str().unwrap().to_string();
    let _ = std::fs::remove_dir_all(&dir);
    seal_bank(&dir);
    let schema = vec![ColMeta::new(1, "k", TypMeta::INT8), ColMeta::new(2, "n", TypMeta::NUMERIC)];
    let bank = Bank::open(&dir, schema, &OpenOpts { bankstats: false, threads: 1 });
    assert_eq!(bank.face(2), Face::PackedNumeric { scale: 2 });
    let engine = Engine::new(bank, SqeConfig { threads: 2, ..SqeConfig::default() });
    assert_eq!(engine.bank.rows_total(), ROWS);
    assert_eq!(engine.faces.stats(&engine.bank, 2).minmax_exact(), Some((125, 996 * 7 + 125)));

    let mut want: Vec<String> =
        expected().iter().map(|&(k, c, s)| format!("{k}\t{c}\t{s}")).collect();
    want.sort();
    let got = run(&engine, vec![AAgg::CountStar, AAgg::Sum { e: AValExpr::Col(2) }]);
    assert_eq!(got, want, "count(*), sum(n) per sparse int8 key");

    let got = run(&engine, vec![AAgg::CountStar, AAgg::Avg { e: AValExpr::Col(2) }]);
    let avg_of = |line: &str| line.rsplit('\t').next().unwrap().to_string();
    let mut by_key: Vec<(String, String)> = got
        .iter()
        .map(|l| (l.split('\t').next().unwrap().to_string(), avg_of(l)))
        .collect();
    by_key.sort();
    let mut want_avg: Vec<(String, String)> = expected()
        .iter()
        .map(|&(k, c, s)| (k.to_string(), (s / c as i128).to_string()))
        .collect();
    want_avg.sort();
    for ((k, got_avg), (wk, whole)) in by_key.iter().zip(&want_avg) {
        assert_eq!(k, wk);
        assert!(
            got_avg.split('.').next() == Some(whole.as_str()),
            "avg(n) mantissa for key {k}: got {got_avg}, want {whole}.x"
        );
    }
    assert_eq!(by_key.len(), GROUPS as usize);
    let _ = std::fs::remove_dir_all(&dir);
}
